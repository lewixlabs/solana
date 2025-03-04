use {
    crate::zk_token_elgamal::pod,
    bytemuck::{Pod, Zeroable},
};
#[cfg(not(target_arch = "bpf"))]
use {
    crate::{
        encryption::{
            discrete_log::*,
            elgamal::{
                DecryptHandle, ElGamalCiphertext, ElGamalKeypair, ElGamalPubkey, ElGamalSecretKey,
            },
            pedersen::{Pedersen, PedersenCommitment, PedersenOpening},
        },
        errors::ProofError,
        instruction::{
            combine_u32_ciphertexts, combine_u32_commitments, combine_u32_openings,
            split_u64_into_u32, transfer::TransferAmountEncryption, Role, Verifiable, TWO_32,
        },
        range_proof::RangeProof,
        sigma_proofs::{
            equality_proof::EqualityProof,
            fee_proof::FeeSigmaProof,
            validity_proof::{AggregatedValidityProof, ValidityProof},
        },
        transcript::TranscriptProtocol,
    },
    arrayref::{array_ref, array_refs},
    curve25519_dalek::scalar::Scalar,
    merlin::Transcript,
    std::convert::TryInto,
    subtle::{ConditionallySelectable, ConstantTimeGreater},
};

#[cfg(not(target_arch = "bpf"))]
const FEE_DENOMINATOR: u64 = 10000;

#[cfg(not(target_arch = "bpf"))]
lazy_static::lazy_static! {
    pub static ref COMMITMENT_FEE_DENOMINATOR: PedersenCommitment = Pedersen::encode(FEE_DENOMINATOR);
}

// #[derive(Clone, Copy, Pod, Zeroable)]
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
pub struct TransferWithFeeData {
    /// Group encryption of the low 32 bites of the transfer amount
    pub ciphertext_lo: pod::TransferAmountEncryption,

    /// Group encryption of the high 32 bits of the transfer amount
    pub ciphertext_hi: pod::TransferAmountEncryption,

    /// The public encryption keys associated with the transfer: source, dest, and auditor
    pub transfer_with_fee_pubkeys: pod::TransferWithFeePubkeys,

    /// The final spendable ciphertext after the transfer,
    pub ciphertext_new_source: pod::ElGamalCiphertext,

    // transfer fee encryption
    pub ciphertext_fee: pod::FeeEncryption,

    // fee parameters
    pub fee_parameters: pod::FeeParameters,

    // transfer fee proof
    pub proof: TransferWithFeeProof,
}

#[cfg(not(target_arch = "bpf"))]
impl TransferWithFeeData {
    pub fn new(
        transfer_amount: u64,
        (spendable_balance, ciphertext_old_source): (u64, &ElGamalCiphertext),
        keypair_source: &ElGamalKeypair,
        (pubkey_dest, pubkey_auditor): (&ElGamalPubkey, &ElGamalPubkey),
        fee_parameters: FeeParameters,
        pubkey_fee_collector: &ElGamalPubkey,
    ) -> Result<Self, ProofError> {
        // split and encrypt transfer amount
        let (amount_lo, amount_hi) = split_u64_into_u32(transfer_amount);

        let (ciphertext_lo, opening_lo) = TransferAmountEncryption::new(
            amount_lo,
            &keypair_source.public,
            pubkey_dest,
            pubkey_auditor,
        );
        let (ciphertext_hi, opening_hi) = TransferAmountEncryption::new(
            amount_hi,
            &keypair_source.public,
            pubkey_dest,
            pubkey_auditor,
        );

        // subtract transfer amount from the spendable ciphertext
        let new_spendable_balance = spendable_balance
            .checked_sub(transfer_amount)
            .ok_or(ProofError::Generation)?;

        let transfer_amount_lo_source = ElGamalCiphertext {
            commitment: ciphertext_lo.commitment,
            handle: ciphertext_lo.source,
        };

        let transfer_amount_hi_source = ElGamalCiphertext {
            commitment: ciphertext_hi.commitment,
            handle: ciphertext_hi.source,
        };

        let ciphertext_new_source = ciphertext_old_source
            - combine_u32_ciphertexts(&transfer_amount_lo_source, &transfer_amount_hi_source);

        // calculate and encrypt fee
        let (fee_amount, delta_fee) =
            calculate_fee(transfer_amount, fee_parameters.fee_rate_basis_points);

        let below_max = u64::ct_gt(&fee_parameters.maximum_fee, &fee_amount);
        let fee_to_encrypt =
            u64::conditional_select(&fee_parameters.maximum_fee, &fee_amount, below_max);
        // u64::conditional_select(&fee_amount, &fee_parameters.maximum_fee, below_max);

        let (ciphertext_fee, opening_fee) =
            FeeEncryption::new(fee_to_encrypt, pubkey_dest, pubkey_fee_collector);

        // generate transcript and append all public inputs
        let pod_transfer_with_fee_pubkeys = pod::TransferWithFeePubkeys::new(
            &keypair_source.public,
            pubkey_dest,
            pubkey_auditor,
            pubkey_fee_collector,
        );
        let pod_ciphertext_lo = pod::TransferAmountEncryption(ciphertext_lo.to_bytes());
        let pod_ciphertext_hi = pod::TransferAmountEncryption(ciphertext_hi.to_bytes());
        let pod_ciphertext_new_source: pod::ElGamalCiphertext = ciphertext_new_source.into();
        let pod_ciphertext_fee = pod::FeeEncryption(ciphertext_fee.to_bytes());

        let mut transcript = TransferWithFeeProof::transcript_new(
            &pod_transfer_with_fee_pubkeys,
            &pod_ciphertext_lo,
            &pod_ciphertext_hi,
            &pod_ciphertext_fee,
        );

        let proof = TransferWithFeeProof::new(
            (amount_lo, &ciphertext_lo, &opening_lo),
            (amount_hi, &ciphertext_hi, &opening_hi),
            keypair_source,
            (pubkey_dest, pubkey_auditor),
            (new_spendable_balance, &ciphertext_new_source),
            (fee_amount, &ciphertext_fee, &opening_fee),
            delta_fee,
            pubkey_fee_collector,
            fee_parameters,
            &mut transcript,
        );

        Ok(Self {
            ciphertext_lo: pod_ciphertext_lo,
            ciphertext_hi: pod_ciphertext_hi,
            transfer_with_fee_pubkeys: pod_transfer_with_fee_pubkeys,
            ciphertext_new_source: pod_ciphertext_new_source,
            ciphertext_fee: pod_ciphertext_fee,
            fee_parameters: fee_parameters.into(),
            proof,
        })
    }

    /// Extracts the lo ciphertexts associated with a transfer-with-fee data
    fn ciphertext_lo(&self, role: Role) -> Result<ElGamalCiphertext, ProofError> {
        let ciphertext_lo: TransferAmountEncryption = self.ciphertext_lo.try_into()?;

        let handle_lo = match role {
            Role::Source => ciphertext_lo.source,
            Role::Dest => ciphertext_lo.dest,
            Role::Auditor => ciphertext_lo.auditor,
        };

        Ok(ElGamalCiphertext {
            commitment: ciphertext_lo.commitment,
            handle: handle_lo,
        })
    }

    /// Extracts the lo ciphertexts associated with a transfer-with-fee data
    fn ciphertext_hi(&self, role: Role) -> Result<ElGamalCiphertext, ProofError> {
        let ciphertext_hi: TransferAmountEncryption = self.ciphertext_hi.try_into()?;

        let handle_hi = match role {
            Role::Source => ciphertext_hi.source,
            Role::Dest => ciphertext_hi.dest,
            Role::Auditor => ciphertext_hi.auditor,
        };

        Ok(ElGamalCiphertext {
            commitment: ciphertext_hi.commitment,
            handle: handle_hi,
        })
    }

    /// Decrypts transfer amount from transfer-with-fee data
    ///
    /// TODO: This function should run in constant time. Use `subtle::Choice` for the if statement
    /// and make sure that the function does not terminate prematurely due to errors
    ///
    /// TODO: Define specific error type for decryption error
    pub fn decrypt_amount(&self, role: Role, sk: &ElGamalSecretKey) -> Result<u64, ProofError> {
        let ciphertext_lo = self.ciphertext_lo(role)?;
        let ciphertext_hi = self.ciphertext_hi(role)?;

        let amount_lo = ciphertext_lo.decrypt_u32_online(sk, &DECODE_U32_PRECOMPUTATION_FOR_G);
        let amount_hi = ciphertext_hi.decrypt_u32_online(sk, &DECODE_U32_PRECOMPUTATION_FOR_G);

        if let (Some(amount_lo), Some(amount_hi)) = (amount_lo, amount_hi) {
            Ok((amount_lo as u64) + (TWO_32 * amount_hi as u64))
        } else {
            Err(ProofError::Verification)
        }
    }
}

#[cfg(not(target_arch = "bpf"))]
impl Verifiable for TransferWithFeeData {
    fn verify(&self) -> Result<(), ProofError> {
        let mut transcript = TransferWithFeeProof::transcript_new(
            &self.transfer_with_fee_pubkeys,
            &self.ciphertext_lo,
            &self.ciphertext_hi,
            &self.ciphertext_fee,
        );

        let ciphertext_lo = self.ciphertext_lo.try_into()?;
        let ciphertext_hi = self.ciphertext_hi.try_into()?;
        let transfer_with_fee_pubkeys = self.transfer_with_fee_pubkeys.try_into()?;
        let new_spendable_ciphertext = self.ciphertext_new_source.try_into()?;

        let ciphertext_fee = self.ciphertext_fee.try_into()?;
        let fee_parameters = self.fee_parameters.into();

        self.proof.verify(
            &ciphertext_lo,
            &ciphertext_hi,
            &transfer_with_fee_pubkeys,
            &new_spendable_ciphertext,
            &ciphertext_fee,
            fee_parameters,
            &mut transcript,
        )
    }
}

// #[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct TransferWithFeeProof {
    pub commitment_new_source: pod::PedersenCommitment,
    pub commitment_claimed: pod::PedersenCommitment,
    pub equality_proof: pod::EqualityProof,
    pub ciphertext_amount_validity_proof: pod::AggregatedValidityProof,
    pub fee_sigma_proof: pod::FeeSigmaProof,
    pub ciphertext_fee_validity_proof: pod::ValidityProof,
    pub range_proof: pod::RangeProof256,
}

#[allow(non_snake_case)]
#[cfg(not(target_arch = "bpf"))]
impl TransferWithFeeProof {
    fn transcript_new(
        transfer_with_fee_pubkeys: &pod::TransferWithFeePubkeys,
        ciphertext_lo: &pod::TransferAmountEncryption,
        ciphertext_hi: &pod::TransferAmountEncryption,
        ciphertext_fee: &pod::FeeEncryption,
    ) -> Transcript {
        let mut transcript = Transcript::new(b"FeeProof");

        transcript.append_message(b"transfer-with-fee-pubkeys", &transfer_with_fee_pubkeys.0);
        transcript.append_message(b"ciphertext-lo", &ciphertext_lo.0);
        transcript.append_message(b"ciphertext-hi", &ciphertext_hi.0);
        transcript.append_message(b"ciphertext-fee", &ciphertext_fee.0);

        transcript
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::many_single_char_names)]
    pub fn new(
        transfer_amount_lo_data: (u32, &TransferAmountEncryption, &PedersenOpening),
        transfer_amount_hi_data: (u32, &TransferAmountEncryption, &PedersenOpening),
        keypair_source: &ElGamalKeypair,
        (pubkey_dest, pubkey_auditor): (&ElGamalPubkey, &ElGamalPubkey),
        (source_new_balance, ciphertext_new_source): (u64, &ElGamalCiphertext),

        (fee_amount, ciphertext_fee, opening_fee): (u64, &FeeEncryption, &PedersenOpening),
        delta_fee: u64,
        pubkey_fee_collector: &ElGamalPubkey,
        fee_parameters: FeeParameters,
        transcript: &mut Transcript,
    ) -> Self {
        let (transfer_amount_lo, ciphertext_lo, opening_lo) = transfer_amount_lo_data;
        let (transfer_amount_hi, ciphertext_hi, opening_hi) = transfer_amount_hi_data;

        // generate a Pedersen commitment for the remaining balance in source
        let (commitment_new_source, opening_source) = Pedersen::new(source_new_balance);
        let (commitment_claimed, opening_claimed) = Pedersen::new(delta_fee);

        let pod_commitment_new_source: pod::PedersenCommitment = commitment_new_source.into();
        let pod_commitment_claimed: pod::PedersenCommitment = commitment_claimed.into();

        transcript.append_commitment(b"commitment-new-source", &pod_commitment_new_source);
        transcript.append_commitment(b"commitment-claimed", &pod_commitment_claimed);

        // generate equality_proof
        let equality_proof = EqualityProof::new(
            keypair_source,
            ciphertext_new_source,
            source_new_balance,
            &opening_source,
            transcript,
        );

        // generate ciphertext validity proof
        let ciphertext_amount_validity_proof = AggregatedValidityProof::new(
            (pubkey_dest, pubkey_auditor),
            (transfer_amount_lo, transfer_amount_hi),
            (opening_lo, opening_hi),
            transcript,
        );

        let (commitment_delta, opening_delta) = compute_delta_commitment_and_opening(
            (&ciphertext_lo.commitment, opening_lo),
            (&ciphertext_hi.commitment, opening_hi),
            (&ciphertext_fee.commitment, opening_fee),
            fee_parameters.fee_rate_basis_points,
        );

        let fee_sigma_proof = FeeSigmaProof::new(
            (fee_amount, &ciphertext_fee.commitment, opening_fee),
            (delta_fee, &commitment_delta, &opening_delta),
            (&commitment_claimed, &opening_claimed),
            fee_parameters.maximum_fee,
            transcript,
        );

        let ciphertext_fee_validity_proof = ValidityProof::new(
            (pubkey_dest, pubkey_fee_collector),
            fee_amount,
            opening_fee,
            transcript,
        );

        let opening_claimed_negated = &PedersenOpening::default() - &opening_claimed;
        let range_proof = RangeProof::new(
            vec![
                source_new_balance,
                transfer_amount_lo as u64,
                transfer_amount_hi as u64,
                delta_fee,
                FEE_DENOMINATOR - delta_fee,
            ],
            vec![
                64, 32, 32, 64, // double check
                64,
            ],
            vec![
                &opening_source,
                opening_lo,
                opening_hi,
                &opening_claimed,
                &opening_claimed_negated,
            ],
            transcript,
        );

        Self {
            commitment_new_source: pod_commitment_new_source,
            commitment_claimed: pod_commitment_claimed,
            equality_proof: equality_proof.into(),
            ciphertext_amount_validity_proof: ciphertext_amount_validity_proof.into(),
            fee_sigma_proof: fee_sigma_proof.into(),
            ciphertext_fee_validity_proof: ciphertext_fee_validity_proof.into(),
            range_proof: range_proof.try_into().expect("range proof: length error"),
        }
    }

    pub fn verify(
        &self,
        ciphertext_lo: &TransferAmountEncryption,
        ciphertext_hi: &TransferAmountEncryption,
        transfer_with_fee_pubkeys: &TransferWithFeePubkeys,
        new_spendable_ciphertext: &ElGamalCiphertext,

        ciphertext_fee: &FeeEncryption,
        fee_parameters: FeeParameters,
        transcript: &mut Transcript,
    ) -> Result<(), ProofError> {
        transcript.append_commitment(b"commitment-new-source", &self.commitment_new_source);
        transcript.append_commitment(b"commitment-claimed", &self.commitment_claimed);

        let commitment_new_source: PedersenCommitment = self.commitment_new_source.try_into()?;
        let commitment_claimed: PedersenCommitment = self.commitment_claimed.try_into()?;

        let equality_proof: EqualityProof = self.equality_proof.try_into()?;
        let ciphertext_amount_validity_proof: AggregatedValidityProof =
            self.ciphertext_amount_validity_proof.try_into()?;
        let fee_sigma_proof: FeeSigmaProof = self.fee_sigma_proof.try_into()?;
        let ciphertext_fee_validity_proof: ValidityProof =
            self.ciphertext_fee_validity_proof.try_into()?;
        let range_proof: RangeProof = self.range_proof.try_into()?;

        // verify equality proof
        equality_proof.verify(
            &transfer_with_fee_pubkeys.source,
            new_spendable_ciphertext,
            &commitment_new_source,
            transcript,
        )?;

        // verify that the transfer amount is encrypted correctly
        ciphertext_amount_validity_proof.verify(
            (
                &transfer_with_fee_pubkeys.dest,
                &transfer_with_fee_pubkeys.auditor,
            ),
            (&ciphertext_lo.commitment, &ciphertext_hi.commitment),
            (&ciphertext_lo.dest, &ciphertext_hi.dest),
            (&ciphertext_lo.auditor, &ciphertext_hi.auditor),
            transcript,
        )?;

        // verify fee sigma proof
        let commitment_delta = compute_delta_commitment(
            &ciphertext_lo.commitment,
            &ciphertext_hi.commitment,
            &ciphertext_fee.commitment,
            fee_parameters.fee_rate_basis_points,
        );

        fee_sigma_proof.verify(
            &ciphertext_fee.commitment,
            &commitment_delta,
            &commitment_claimed,
            fee_parameters.maximum_fee,
            transcript,
        )?;

        ciphertext_fee_validity_proof.verify(
            &ciphertext_fee.commitment,
            (
                &transfer_with_fee_pubkeys.dest,
                &transfer_with_fee_pubkeys.fee_collector,
            ),
            (&ciphertext_fee.dest, &ciphertext_fee.fee_collector),
            transcript,
        )?;

        let commitment_claimed_negated = &(*COMMITMENT_FEE_DENOMINATOR) - &commitment_claimed;
        range_proof.verify(
            vec![
                &commitment_new_source,
                &ciphertext_lo.commitment,
                &ciphertext_hi.commitment,
                &commitment_claimed,
                &commitment_claimed_negated,
            ],
            vec![64, 32, 32, 64, 64],
            transcript,
        )?;

        Ok(())
    }
}

/// The ElGamal public keys needed for a transfer with fee
#[derive(Clone)]
#[repr(C)]
#[cfg(not(target_arch = "bpf"))]
pub struct TransferWithFeePubkeys {
    pub source: ElGamalPubkey,
    pub dest: ElGamalPubkey,
    pub auditor: ElGamalPubkey,
    pub fee_collector: ElGamalPubkey,
}

#[cfg(not(target_arch = "bpf"))]
impl TransferWithFeePubkeys {
    pub fn to_bytes(&self) -> [u8; 128] {
        let mut bytes = [0u8; 128];
        bytes[..32].copy_from_slice(&self.source.to_bytes());
        bytes[32..64].copy_from_slice(&self.dest.to_bytes());
        bytes[64..96].copy_from_slice(&self.auditor.to_bytes());
        bytes[96..128].copy_from_slice(&self.fee_collector.to_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProofError> {
        let bytes = array_ref![bytes, 0, 128];
        let (source, dest, auditor, fee_collector) = array_refs![bytes, 32, 32, 32, 32];

        let source = ElGamalPubkey::from_bytes(source).ok_or(ProofError::Verification)?;
        let dest = ElGamalPubkey::from_bytes(dest).ok_or(ProofError::Verification)?;
        let auditor = ElGamalPubkey::from_bytes(auditor).ok_or(ProofError::Verification)?;
        let fee_collector =
            ElGamalPubkey::from_bytes(fee_collector).ok_or(ProofError::Verification)?;

        Ok(Self {
            source,
            dest,
            auditor,
            fee_collector,
        })
    }
}

#[cfg(not(target_arch = "bpf"))]
impl pod::TransferWithFeePubkeys {
    pub fn new(
        source: &ElGamalPubkey,
        dest: &ElGamalPubkey,
        auditor: &ElGamalPubkey,
        fee_collector: &ElGamalPubkey,
    ) -> Self {
        let mut bytes = [0u8; 128];
        bytes[..32].copy_from_slice(&source.to_bytes());
        bytes[32..64].copy_from_slice(&dest.to_bytes());
        bytes[64..96].copy_from_slice(&auditor.to_bytes());
        bytes[96..128].copy_from_slice(&fee_collector.to_bytes());
        Self(bytes)
    }
}

#[derive(Clone)]
#[repr(C)]
#[cfg(not(target_arch = "bpf"))]
pub struct FeeEncryption {
    pub commitment: PedersenCommitment,
    pub dest: DecryptHandle,
    pub fee_collector: DecryptHandle,
}

#[cfg(not(target_arch = "bpf"))]
impl FeeEncryption {
    pub fn new(
        amount: u64,
        pubkey_dest: &ElGamalPubkey,
        pubkey_fee_collector: &ElGamalPubkey,
    ) -> (Self, PedersenOpening) {
        let (commitment, opening) = Pedersen::new(amount);
        let fee_encryption = Self {
            commitment,
            dest: pubkey_dest.decrypt_handle(&opening),
            fee_collector: pubkey_fee_collector.decrypt_handle(&opening),
        };

        (fee_encryption, opening)
    }

    pub fn to_bytes(&self) -> [u8; 96] {
        let mut bytes = [0u8; 96];
        bytes[..32].copy_from_slice(&self.commitment.to_bytes());
        bytes[32..64].copy_from_slice(&self.dest.to_bytes());
        bytes[64..96].copy_from_slice(&self.fee_collector.to_bytes());
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProofError> {
        let bytes = array_ref![bytes, 0, 96];
        let (commitment, dest, fee_collector) = array_refs![bytes, 32, 32, 32];

        let commitment =
            PedersenCommitment::from_bytes(commitment).ok_or(ProofError::Verification)?;
        let dest = DecryptHandle::from_bytes(dest).ok_or(ProofError::Verification)?;
        let fee_collector =
            DecryptHandle::from_bytes(fee_collector).ok_or(ProofError::Verification)?;

        Ok(Self {
            commitment,
            dest,
            fee_collector,
        })
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct FeeParameters {
    /// Fee rate expressed as basis points of the transfer amount, i.e. increments of 0.01%
    pub fee_rate_basis_points: u16,
    /// Maximum fee assessed on transfers, expressed as an amount of tokens
    pub maximum_fee: u64,
}

#[cfg(not(target_arch = "bpf"))]
impl FeeParameters {
    pub fn to_bytes(&self) -> [u8; 10] {
        let mut bytes = [0u8; 10];
        bytes[..2].copy_from_slice(&self.fee_rate_basis_points.to_le_bytes());
        bytes[2..10].copy_from_slice(&self.maximum_fee.to_le_bytes());

        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Self {
        let bytes = array_ref![bytes, 0, 10];
        let (fee_rate_basis_points, maximum_fee) = array_refs![bytes, 2, 8];

        Self {
            fee_rate_basis_points: u16::from_le_bytes(*fee_rate_basis_points),
            maximum_fee: u64::from_le_bytes(*maximum_fee),
        }
    }
}

#[cfg(not(target_arch = "bpf"))]
fn calculate_fee(transfer_amount: u64, fee_rate_basis_points: u16) -> (u64, u64) {
    let fee_scaled = (transfer_amount as u128) * (fee_rate_basis_points as u128);

    let fee = (fee_scaled / FEE_DENOMINATOR as u128) as u64;
    let rem = (fee_scaled % FEE_DENOMINATOR as u128) as u64;

    if rem == 0 {
        (fee, rem)
    } else {
        (fee + 1, rem)
    }
}

#[cfg(not(target_arch = "bpf"))]
fn compute_delta_commitment_and_opening(
    (commitment_lo, opening_lo): (&PedersenCommitment, &PedersenOpening),
    (commitment_hi, opening_hi): (&PedersenCommitment, &PedersenOpening),
    (commitment_fee, opening_fee): (&PedersenCommitment, &PedersenOpening),
    fee_rate_basis_points: u16,
) -> (PedersenCommitment, PedersenOpening) {
    let fee_rate_scalar = Scalar::from(fee_rate_basis_points);

    let commitment_delta = commitment_fee * Scalar::from(FEE_DENOMINATOR)
        - &(&combine_u32_commitments(commitment_lo, commitment_hi) * &fee_rate_scalar);

    let opening_delta = opening_fee * Scalar::from(FEE_DENOMINATOR)
        - &(&combine_u32_openings(opening_lo, opening_hi) * &fee_rate_scalar);

    (commitment_delta, opening_delta)
}

#[cfg(not(target_arch = "bpf"))]
fn compute_delta_commitment(
    commitment_lo: &PedersenCommitment,
    commitment_hi: &PedersenCommitment,
    commitment_fee: &PedersenCommitment,
    fee_rate_basis_points: u16,
) -> PedersenCommitment {
    let fee_rate_scalar = Scalar::from(fee_rate_basis_points);

    commitment_fee * Scalar::from(FEE_DENOMINATOR)
        - &(&combine_u32_commitments(commitment_lo, commitment_hi) * &fee_rate_scalar)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_fee_correctness() {
        let keypair_source = ElGamalKeypair::new_rand();
        let pubkey_dest = ElGamalKeypair::new_rand().public;
        let pubkey_auditor = ElGamalKeypair::new_rand().public;
        let pubkey_fee_collector = ElGamalKeypair::new_rand().public;

        let spendable_balance: u64 = 120;
        let spendable_ciphertext = keypair_source.public.encrypt(spendable_balance);

        let transfer_amount: u64 = 100;

        let fee_parameters = FeeParameters {
            fee_rate_basis_points: 100,
            maximum_fee: 3,
        };

        let fee_data = TransferWithFeeData::new(
            transfer_amount,
            (spendable_balance, &spendable_ciphertext),
            &keypair_source,
            (&pubkey_dest, &pubkey_auditor),
            fee_parameters,
            &pubkey_fee_collector,
        )
        .unwrap();

        assert!(fee_data.verify().is_ok());
    }
}
