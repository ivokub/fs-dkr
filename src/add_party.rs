use crate::error::{FsDkrError, FsDkrResult};
use crate::refresh_message::RefreshMessage;
use curv::arithmetic::{BasicOps, Modulo, One, Samplable, Zero};
use curv::cryptographic_primitives::secret_sharing::feldman_vss::{
    ShamirSecretSharing, VerifiableSS,
};
use curv::elliptic::curves::traits::{ECPoint, ECScalar};
use curv::BigInt;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::party_i::Keys;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::party_i::SharedKeys;
use multi_party_ecdsa::protocols::multi_party_ecdsa::gg_2020::state_machine::keygen::LocalKey;
pub use paillier::DecryptionKey;
use paillier::{Decrypt, EncryptionKey, KeyGeneration, Paillier};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;
use zeroize::Zeroize;
use zk_paillier::zkproofs::{CompositeDLogProof, DLogStatement, NICorrectKeyProof};

// Everything here can be broadcasted
#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct JoinMessage {
    pub(crate) ek: EncryptionKey,
    pub(crate) dk_correctness_proof: NICorrectKeyProof,
    pub(crate) party_index: Option<usize>,
    pub(crate) dlog_statement_base_h1: DLogStatement,
    pub(crate) dlog_statement_base_h2: DLogStatement,
    pub(crate) composite_dlog_proof_base_h1: CompositeDLogProof,
    pub(crate) composite_dlog_proof_base_h2: CompositeDLogProof,
}

fn generate_h1_h2_n_tilde() -> (BigInt, BigInt, BigInt, BigInt, BigInt) {
    let (ek_tilde, dk_tilde) = Paillier::keypair().keys();
    let one = BigInt::one();
    let phi = (&dk_tilde.p - &one) * (&dk_tilde.q - &one);
    let h1 = BigInt::sample_below(&ek_tilde.n);
    let (mut xhi, mut xhi_inv) = loop {
        let xhi_ = BigInt::sample_below(&phi);
        match BigInt::mod_inv(&xhi_, &phi) {
            Some(inv) => break (xhi_, inv),
            None => continue,
        }
    };
    let h2 = BigInt::mod_pow(&h1, &xhi, &ek_tilde.n);
    xhi = BigInt::sub(&phi, &xhi);
    xhi_inv = BigInt::sub(&phi, &xhi_inv);
    (ek_tilde.n, h1, h2, xhi, xhi_inv)
}

fn generate_dlog_statement_proofs() -> (
    DLogStatement,
    DLogStatement,
    CompositeDLogProof,
    CompositeDLogProof,
) {
    let (n_tilde, h1, h2, xhi, xhi_inv) = generate_h1_h2_n_tilde();

    let dlog_statement_base_h1 = DLogStatement {
        N: n_tilde.clone(),
        g: h1.clone(),
        ni: h2.clone(),
    };

    let dlog_statement_base_h2 = DLogStatement {
        N: n_tilde,
        g: h2,
        ni: h1,
    };

    let composite_dlog_proof_base_h1 = CompositeDLogProof::prove(&dlog_statement_base_h1, &xhi);
    let composite_dlog_proof_base_h2 = CompositeDLogProof::prove(&dlog_statement_base_h2, &xhi_inv);

    (
        dlog_statement_base_h1,
        dlog_statement_base_h2,
        composite_dlog_proof_base_h1,
        composite_dlog_proof_base_h2,
    )
}

impl JoinMessage {
    pub fn distribute() -> (Self, Keys) {
        let pailier_key_pair = Keys::create(0);
        let (
            dlog_statement_base_h1,
            dlog_statement_base_h2,
            composite_dlog_proof_base_h1,
            composite_dlog_proof_base_h2,
        ) = generate_dlog_statement_proofs();

        let join_message = JoinMessage {
            // in a join message, we only care about the ek and the correctness proof
            ek: pailier_key_pair.ek.clone(),
            dk_correctness_proof: NICorrectKeyProof::proof(&pailier_key_pair.dk, None),
            dlog_statement_base_h1,
            dlog_statement_base_h2,
            composite_dlog_proof_base_h1,
            composite_dlog_proof_base_h2,
            party_index: None,
        };

        (join_message, pailier_key_pair)
    }

    pub fn get_party_index(&self) -> FsDkrResult<usize> {
        self.party_index
            .ok_or(FsDkrError::NewPartyUnassignedIndexError)
    }

    pub fn collect<P>(
        &self,
        refresh_messages: &[RefreshMessage<P>],
        paillier_key: Keys,
        join_messages: &[JoinMessage],
        t: usize,
        n: usize,
    ) -> FsDkrResult<LocalKey<P>>
    where
        P: ECPoint + Clone + Zeroize + Debug,
        P::Scalar: PartialEq + Clone + Debug + Zeroize,
    {
        RefreshMessage::validate_collect(refresh_messages, t, n)?;
        let party_index = self.party_index.unwrap();

        for join_message in join_messages.iter() {
            join_message.get_party_index()?;
        }

        let parameters = ShamirSecretSharing {
            threshold: t,
            share_count: n,
        };

        let (cipher_text_sum, li_vec) = RefreshMessage::get_ciphertext_sum(
            refresh_messages,
            party_index,
            &parameters,
            &paillier_key.ek,
        );
        let new_share = Paillier::decrypt(&paillier_key.dk, cipher_text_sum)
            .0
            .into_owned();

        let new_share_fe: P::Scalar = ECScalar::from(&new_share);
        let paillier_dk = paillier_key.dk.clone();
        let key_linear_x_i = new_share_fe.clone();
        let key_linear_y = P::generator() * new_share_fe.clone();
        let keys_linear = SharedKeys {
            x_i: key_linear_x_i,
            y: key_linear_y,
        };
        let mut pk_vec: Vec<_> = (0..n)
            .map(|i| refresh_messages[0].points_committed_vec[i].clone() * li_vec[0].clone())
            .collect();

        for i in 0..n as usize {
            for j in 1..(t as usize + 1) {
                pk_vec[i] = pk_vec[i].clone()
                    + refresh_messages[j].points_committed_vec[i].clone() * li_vec[j].clone();
            }
        }

        let available_parties: HashMap<usize, &EncryptionKey> = refresh_messages
            .iter()
            .map(|msg| (msg.party_index, &msg.ek))
            .chain(std::iter::once((party_index, &paillier_key.ek)))
            .chain(
                join_messages
                    .iter()
                    .map(|join_message| (join_message.party_index.unwrap(), &join_message.ek)),
            )
            .collect();

        // TODO: submit the statement the dlog proof as well!
        let available_h1_h2_ntilde_vec: HashMap<usize, &DLogStatement> = refresh_messages
            .iter()
            .map(|msg| (msg.party_index, &msg.dlog_statement))
            .chain(std::iter::once((party_index, &self.dlog_statement_base_h1)))
            .chain(join_messages.iter().map(|join_message| {
                (
                    join_message.party_index.unwrap(),
                    &join_message.dlog_statement_base_h1,
                )
            }))
            .collect();

        let paillier_key_vec: Vec<EncryptionKey> = (1..n + 1)
            .map(|party| {
                let ek = available_parties.get(&party);

                match ek {
                    None => EncryptionKey {
                        n: BigInt::zero(),
                        nn: BigInt::zero(),
                    },
                    Some(key) => (*key).clone(),
                }
            })
            .collect();

        let h1_h2_ntilde_vec: Vec<DLogStatement> = (1..n + 1)
            .map(|party| {
                let statement = available_h1_h2_ntilde_vec.get(&party);

                match statement {
                    None => generate_dlog_statement_proofs().0,
                    Some(dlog_statement) => (*dlog_statement).clone(),
                }
            })
            .collect();

        for refresh_message in refresh_messages.iter() {
            if refresh_message.public_key != refresh_messages[0].public_key {
                return Err(FsDkrError::BroadcastedPublicKeyError);
            }
        }

        // secret share old key
        let (vss_scheme, _) = VerifiableSS::<P>::share(t, n, &new_share_fe);

        let local_key = LocalKey {
            paillier_dk,
            pk_vec,
            keys_linear,
            paillier_key_vec,
            y_sum_s: refresh_messages[0].public_key.clone(),
            h1_h2_n_tilde_vec: h1_h2_ntilde_vec,
            vss_scheme,
            i: party_index as u16,
            t: t as u16,
            n: n as u16,
        };

        Ok(local_key)
    }
}
