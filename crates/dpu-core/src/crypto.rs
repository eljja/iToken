use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use crate::types::InferenceReceipt;

pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
    use rand::RngCore;
    let mut secret_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret_bytes);
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let verifying_key = signing_key.verifying_key();
    (signing_key, verifying_key)
}

pub fn pubkey_to_hex(verifying_key: &VerifyingKey) -> String {
    hex::encode(verifying_key.to_bytes())
}

pub fn hex_to_pubkey(hex_str: &str) -> Result<VerifyingKey, String> {
    let bytes = hex::decode(hex_str).map_err(|e| e.to_string())?;
    let array: [u8; 32] = bytes.try_into().map_err(|_| "Invalid public key length".to_string())?;
    VerifyingKey::from_bytes(&array).map_err(|e| e.to_string())
}

pub fn sign_bytes(signing_key: &SigningKey, message: &[u8]) -> String {
    use ed25519_dalek::Signer;
    let signature = signing_key.sign(message);
    hex::encode(signature.to_bytes())
}

pub fn verify_signature(pubkey_hex: &str, message: &[u8], signature_hex: &str) -> bool {
    let verifying_key = match hex_to_pubkey(pubkey_hex) {
        Ok(k) => k,
        Err(_) => return false,
    };
    
    let sig_bytes = match hex::decode(signature_hex) {
        Ok(b) => b,
        Err(_) => return false,
    };
    
    let sig_array: [u8; 64] = match sig_bytes.try_into() {
        Ok(a) => a,
        Err(_) => return false,
    };
    
    let signature = Signature::from_bytes(&sig_array);
    verifying_key.verify(message, &signature).is_ok()
}

pub fn sha256_hash(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn get_receipt_signing_bytes(receipt: &InferenceReceipt) -> Vec<u8> {
    // Standardized serialization for signing
    format!(
        "{}:{}:{}:{}:{}:{:.4}:{:.4}:{:.4}:{}",
        receipt.receipt_id,
        receipt.client_pubkey,
        receipt.node_pubkey,
        receipt.query_hash,
        receipt.tokens_generated,
        receipt.tps,
        receipt.tqw,
        receipt.amount_itokens,
        receipt.timestamp
    )
    .into_bytes()
}

pub fn sign_receipt_as_node(signing_key: &SigningKey, receipt: &mut InferenceReceipt) {
    let msg = get_receipt_signing_bytes(receipt);
    let sig = sign_bytes(signing_key, &msg);
    receipt.node_signature = Some(sig);
}

pub fn sign_receipt_as_client(signing_key: &SigningKey, receipt: &mut InferenceReceipt) {
    let msg = get_receipt_signing_bytes(receipt);
    let sig = sign_bytes(signing_key, &msg);
    receipt.client_signature = Some(sig);
}

pub fn verify_receipt_signatures(receipt: &InferenceReceipt) -> bool {
    let msg = get_receipt_signing_bytes(receipt);
    
    let node_ok = match &receipt.node_signature {
        Some(sig) => verify_signature(&receipt.node_pubkey, &msg, sig),
        None => false,
    };
    
    let client_ok = match &receipt.client_signature {
        Some(sig) => verify_signature(&receipt.client_pubkey, &msg, sig),
        None => false,
    };
    
    node_ok && client_ok
}
