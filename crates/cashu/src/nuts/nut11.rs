//! Pay to Public Key (P2PK)
// https://github.com/cashubtc/nuts/blob/main/11.md

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::Hash;
use k256::schnorr::signature::{Signer, Verifier};
use k256::schnorr::{Signature, SigningKey, VerifyingKey};
use serde::de::Error as DeserializerError;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::nut01::PublicKey;
use super::nut02::Id;
use super::nut10::{Secret, SecretData, UncheckedSecret};
use crate::error::Error;
use crate::utils::unix_time;
use crate::Amount;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signatures {
    signatures: Vec<String>,
}

/// Proofs [NUT-11]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proof {
    /// Amount in satoshi
    pub amount: Amount,
    /// NUT-10 Secret
    pub secret: UncheckedSecret,
    /// Unblinded signature
    #[serde(rename = "C")]
    pub c: PublicKey,
    /// `Keyset id`
    pub id: Option<Id>,
    /// Witness
    pub witness: Signatures,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2PKConditions {
    pub locktime: Option<u64>,
    pub pubkeys: Vec<PublicKey>,
    pub refund_keys: Option<Vec<PublicKey>>,
    pub num_sigs: Option<u64>,
    pub sig_flag: SigFlag,
}

impl TryFrom<P2PKConditions> for Secret {
    type Error = Error;
    fn try_from(conditions: P2PKConditions) -> Result<Secret, Self::Error> {
        let P2PKConditions {
            locktime,
            pubkeys,
            refund_keys,
            num_sigs,
            sig_flag,
        } = conditions;

        // Check there is at least one pubkey
        if pubkeys.len().lt(&1) {
            return Err(Error::Amount);
        }

        let data = pubkeys[0].to_hex();

        let mut tags = vec![];

        if pubkeys.len().gt(&1) {
            tags.push(Tag::PubKeys(pubkeys.into_iter().skip(1).collect()).as_vec());
        }

        if let Some(locktime) = locktime {
            tags.push(Tag::LockTime(locktime).as_vec());
        }

        if let Some(num_sigs) = num_sigs {
            tags.push(Tag::NSigs(num_sigs).as_vec());
        }

        if let Some(refund_keys) = refund_keys {
            tags.push(Tag::Refund(refund_keys).as_vec())
        }

        tags.push(Tag::SigFlag(sig_flag).as_vec());

        let tags = if tags.len().gt(&0) { Some(tags) } else { None };

        Ok(Secret {
            kind: super::nut10::Kind::P2PK,
            secret_data: SecretData {
                nonce: crate::secret::Secret::default().to_string(),
                data,
                tags,
            },
        })
    }
}

impl TryFrom<Secret> for P2PKConditions {
    type Error = Error;
    fn try_from(secret: Secret) -> Result<P2PKConditions, Self::Error> {
        let tags: HashMap<TagKind, Tag> = secret
            .clone()
            .secret_data
            .tags
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                let tag = Tag::try_from(t).unwrap();
                (tag.kind(), tag)
            })
            .collect();

        let mut pubkeys: Vec<PublicKey> = vec![];

        if let Some(tag) = tags.get(&TagKind::Pubkeys) {
            match tag {
                Tag::PubKeys(keys) => {
                    let mut keys = keys.clone();
                    pubkeys.append(&mut keys);
                }
                _ => (),
            }
        }

        let data_pubkey = PublicKey::from_hex(secret.secret_data.data)?;
        pubkeys.push(data_pubkey);

        let locktime = if let Some(tag) = tags.get(&TagKind::Locktime) {
            match tag {
                Tag::LockTime(locktime) => Some(*locktime),
                _ => None,
            }
        } else {
            None
        };

        let refund_keys = if let Some(tag) = tags.get(&TagKind::Refund) {
            match tag {
                Tag::Refund(keys) => Some(keys.clone()),
                _ => None,
            }
        } else {
            None
        };

        let sig_flag = if let Some(tag) = tags.get(&TagKind::SigFlag) {
            match tag {
                Tag::SigFlag(sigflag) => sigflag.clone(),
                _ => SigFlag::SigInputs,
            }
        } else {
            SigFlag::SigInputs
        };

        let num_sigs = if let Some(tag) = tags.get(&TagKind::NSigs) {
            match tag {
                Tag::NSigs(num_sigs) => Some(*num_sigs),
                _ => None,
            }
        } else {
            None
        };

        Ok(P2PKConditions {
            locktime,
            pubkeys,
            refund_keys,
            num_sigs,
            sig_flag,
        })
    }
}

impl Proof {
    pub fn verify_p2pk(&self) -> Result<(), Error> {
        let secret: Secret = (&self.secret).try_into().unwrap();
        if secret.kind.ne(&super::nut10::Kind::P2PK) {
            return Err(Error::IncorrectSecretKind);
        }

        let spending_conditions: P2PKConditions = secret.clone().try_into()?;

        let mut valid_sigs = 0;

        let msg = Sha256::hash(self.secret.as_bytes());

        for signature in &self.witness.signatures {
            let mut pubkeys = spending_conditions.pubkeys.clone();
            let data_key = PublicKey::from_str(&secret.secret_data.data).unwrap();
            pubkeys.push(data_key);
            for v in &spending_conditions.pubkeys {
                let sig = Signature::try_from(hex::decode(signature).unwrap().as_slice()).unwrap();

                let verifying_key: VerifyingKey = v.try_into()?;

                if verifying_key.verify(&msg.to_byte_array(), &sig).is_ok() {
                    valid_sigs += 1;
                } else {
                    println!(
                        "{:?}",
                        verifying_key.verify(&msg.to_byte_array(), &sig).unwrap()
                    );
                }
            }
        }

        if valid_sigs.ge(&spending_conditions.num_sigs.unwrap_or(1)) {
            return Ok(());
        }

        if let Some(locktime) = spending_conditions.locktime {
            // If lock time has passed check if refund witness signature is valid
            if locktime.lt(&unix_time()) {
                if let Some(refund_pubkeys) = &spending_conditions.refund_keys {
                    for s in &self.witness.signatures {
                        for v in refund_pubkeys {
                            let sig = Signature::try_from(s.as_bytes())
                                .map_err(|_| Error::InvalidSignature)?;
                            let v: VerifyingKey = v.clone().try_into()?;

                            // As long as there is one valid refund signature it can be spent
                            if v.verify(&msg.to_byte_array(), &sig).is_ok() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        Err(Error::SpendConditionsNotMet)
    }

    pub fn sign_p2pk_proof(&mut self, secret_key: SigningKey) -> Result<(), Error> {
        let msg_to_sign = Sha256::hash(&self.secret.as_bytes());

        let signature = secret_key.sign(msg_to_sign.as_byte_array());

        self.witness
            .signatures
            .push(hex::encode(signature.to_bytes()));
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum TagKind {
    /// Signature flag
    SigFlag,
    /// Number signatures required
    #[serde(rename = "n_sigs")]
    NSigs,
    /// Locktime
    Locktime,
    /// Refund
    Refund,
    /// Pubkey
    Pubkeys,
    /// Custom tag kind
    Custom(String),
}

impl fmt::Display for TagKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SigFlag => write!(f, "sigflag"),
            Self::NSigs => write!(f, "n_sigs"),
            Self::Locktime => write!(f, "locktime"),
            Self::Refund => write!(f, "refund"),
            Self::Pubkeys => write!(f, "pubkeys"),
            Self::Custom(kind) => write!(f, "{}", kind),
        }
    }
}

impl<S> From<S> for TagKind
where
    S: AsRef<str>,
{
    fn from(tag: S) -> Self {
        match tag.as_ref() {
            "sigflag" => Self::SigFlag,
            "n_sigs" => Self::NSigs,
            "locktime" => Self::Locktime,
            "refund" => Self::Refund,
            "pubkeys" => Self::Pubkeys,
            t => Self::Custom(t.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord, Hash)]
pub enum SigFlag {
    SigAll,
    SigInputs,
    Custom(String),
}

impl fmt::Display for SigFlag {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SigAll => write!(f, "SIG_ALL"),
            Self::SigInputs => write!(f, "SIG_INPUTS"),
            Self::Custom(flag) => write!(f, "{}", flag),
        }
    }
}

impl<S> From<S> for SigFlag
where
    S: AsRef<str>,
{
    fn from(tag: S) -> Self {
        match tag.as_ref() {
            "SIG_ALL" => Self::SigAll,
            "SIG_INPUTS" => Self::SigInputs,
            tag => Self::Custom(tag.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag {
    SigFlag(SigFlag),
    NSigs(u64),
    LockTime(u64),
    Refund(Vec<PublicKey>),
    PubKeys(Vec<PublicKey>),
}

impl Tag {
    pub fn kind(&self) -> TagKind {
        match self {
            Self::SigFlag(_) => TagKind::SigFlag,
            Self::NSigs(_) => TagKind::NSigs,
            Self::LockTime(_) => TagKind::Locktime,
            Self::Refund(_) => TagKind::Refund,
            Self::PubKeys(_) => TagKind::Pubkeys,
        }
    }

    /// Get [`Tag`] as string vector
    pub fn as_vec(&self) -> Vec<String> {
        self.clone().into()
    }
}

impl<S> TryFrom<Vec<S>> for Tag
where
    S: AsRef<str>,
{
    type Error = Error;

    fn try_from(tag: Vec<S>) -> Result<Self, Self::Error> {
        let tag_len = tag.len();
        let tag_kind: TagKind = match tag.first() {
            Some(kind) => TagKind::from(kind),
            None => return Err(Error::KindNotFound),
        };

        if tag_len.eq(&2) {
            match tag_kind {
                TagKind::SigFlag => Ok(Tag::SigFlag(SigFlag::from(tag[1].as_ref()))),
                TagKind::NSigs => Ok(Tag::NSigs(tag[1].as_ref().parse().unwrap())),
                TagKind::Locktime => Ok(Tag::LockTime(tag[1].as_ref().parse().unwrap())),
                _ => Err(Error::UnknownTag),
            }
        } else if tag_len.gt(&1) {
            match tag_kind {
                TagKind::Refund => {
                    let pubkeys = tag
                        .iter()
                        .skip(1)
                        .map(|p| PublicKey::from_hex(p.as_ref().to_string()))
                        .flatten()
                        .collect();

                    Ok(Self::Refund(pubkeys))
                }
                TagKind::Pubkeys => {
                    let pubkeys = tag
                        .iter()
                        .skip(1)
                        .map(|p| PublicKey::from_hex(p.as_ref().to_string()))
                        .flatten()
                        .collect();

                    Ok(Self::PubKeys(pubkeys))
                }
                _ => Err(Error::UnknownTag),
            }
        } else {
            Err(Error::UnknownTag)
        }
    }
}

impl From<Tag> for Vec<String> {
    fn from(data: Tag) -> Self {
        match data {
            Tag::SigFlag(sigflag) => vec![TagKind::SigFlag.to_string(), sigflag.to_string()],
            Tag::NSigs(num_sig) => vec![TagKind::NSigs.to_string(), num_sig.to_string()],
            Tag::LockTime(locktime) => vec![TagKind::Locktime.to_string(), locktime.to_string()],
            Tag::PubKeys(pubkeys) => {
                let mut tag = vec![TagKind::Pubkeys.to_string()];

                for pubkey in pubkeys {
                    tag.push(pubkey.to_hex())
                }
                tag
            }
            Tag::Refund(pubkeys) => {
                let mut tag = vec![TagKind::Refund.to_string()];

                for pubkey in pubkeys {
                    tag.push(pubkey.to_hex())
                }
                tag
            }
        }
    }
}

impl Serialize for Tag {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let data: Vec<String> = self.as_vec();
        let mut seq = serializer.serialize_seq(Some(data.len()))?;
        for element in data.into_iter() {
            seq.serialize_element(&element)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Tag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        type Data = Vec<String>;
        let vec: Vec<String> = Data::deserialize(deserializer)?;
        Self::try_from(vec).map_err(DeserializerError::custom)
    }
}

#[cfg(test)]
mod tests {

    use std::str::FromStr;

    use super::*;
    use crate::nuts::SecretKey;

    #[test]
    fn test_secret_ser() {
        let conditions = P2PKConditions {
            locktime: Some(99999),
            pubkeys: vec![
                PublicKey::from_str(
                    "033281c37677ea273eb7183b783067f5244933ef78d8c3f15b1a77cb246099c26e",
                )
                .unwrap(),
                PublicKey::from_str(
                    "02698c4e2b5f9534cd0687d87513c759790cf829aa5739184a3e3735471fbda904",
                )
                .unwrap(),
                PublicKey::from_str(
                    "023192200a0cfd3867e48eb63b03ff599c7e46c8f4e41146b2d281173ca6c50c54",
                )
                .unwrap(),
            ],
            refund_keys: Some(vec![PublicKey::from_str(
                "033281c37677ea273eb7183b783067f5244933ef78d8c3f15b1a77cb246099c26e",
            )
            .unwrap()]),
            num_sigs: Some(2),
            sig_flag: SigFlag::SigAll,
        };

        let secret: Secret = conditions.try_into().unwrap();

        let secret_str = serde_json::to_string(&secret).unwrap();

        let secret_der: Secret = serde_json::from_str(&secret_str).unwrap();

        assert_eq!(secret_der, secret);
    }

    #[test]
    fn test_verify() {
        let proof_str = r#"{"amount":0,"secret":"[\"P2PK\",{\"nonce\":\"190badde56afcbf67937e228744ea896bb3e48bcb60efa412799e1518618c287\",\"data\":\"0249098aa8b9d2fbec49ff8598feb17b592b986e62319a4fa488a3dc36387157a7\",\"tags\":[[\"sigflag\",\"SIG_INPUTS\"]]}]","C":"02698c4e2b5f9534cd0687d87513c759790cf829aa5739184a3e3735471fbda904","id":null,"witness":{"signatures":["2b117c29a0e405fcbcac4c632b5862eb3ace0d67c681e8209d3aa2f52d5198471629b1ec6bce75d3879c47725be89d28938e31236307b40bc6c89491fa540e35"]}}"#;

        let proof: Proof = serde_json::from_str(proof_str).unwrap();

        assert!(proof.verify_p2pk().is_ok());
    }

    #[test]
    fn sign_proof() {
        let secret_key =
            SecretKey::from_hex("04918dfc36c93e7db6cc0d60f37e1522f1c36b64d3f4b424c532d7c595febbc5")
                .unwrap();

        let pubkey: PublicKey = secret_key.public_key();

        let v_key: VerifyingKey = pubkey.clone().try_into().unwrap();

        let conditions = P2PKConditions {
            locktime: None,
            pubkeys: vec![v_key.into()],
            refund_keys: None,
            num_sigs: None,
            sig_flag: SigFlag::SigInputs,
        };

        let secret: super::Secret = conditions.try_into().unwrap();

        let mut proof = Proof {
            id: None,
            amount: Amount::ZERO,
            secret: secret.try_into().unwrap(),
            c: PublicKey::from_str(
                "02698c4e2b5f9534cd0687d87513c759790cf829aa5739184a3e3735471fbda904",
            )
            .unwrap(),
            witness: Signatures { signatures: vec![] },
        };

        let signing_key: SigningKey = secret_key.try_into().unwrap();

        proof.sign_p2pk_proof(signing_key).unwrap();

        assert!(proof.verify_p2pk().is_ok());
    }
}
