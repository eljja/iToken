use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use crate::types::InferenceReceipt;

// ─── Key Generation ────────────────────────────────────────────────────────────

pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
    use rand::RngCore;
    let mut secret_bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret_bytes);
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let verifying_key = signing_key.verifying_key();
    (signing_key, verifying_key)
}

pub fn pubkey_to_hex(verifying_key: &VerifyingKey) -> String {
    hex::encode(verifying_key.to_bytes())
}

pub fn hex_to_pubkey(hex_str: &str) -> Result<VerifyingKey, String> {
    let bytes = hex::decode(hex_str).map_err(|e| format!("Invalid hex: {}", e))?;
    if bytes.len() != 32 {
        return Err(format!("Invalid public key length: {} (expected 32)", bytes.len()));
    }
    let array: [u8; 32] = bytes.try_into().map_err(|_| "Invalid public key length".to_string())?;
    VerifyingKey::from_bytes(&array).map_err(|e| format!("Invalid public key: {}", e))
}

// ─── Key Persistence ───────────────────────────────────────────────────────────

/// Load an existing keypair from file, or generate and save a new one.
/// The file stores the 32-byte Ed25519 secret key.
pub fn load_or_generate_keypair(path: &std::path::Path) -> Result<(SigningKey, VerifyingKey), String> {
    if path.exists() {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("Failed to read key file '{}': {}", path.display(), e))?;
        if bytes.len() != 32 {
            return Err(format!(
                "Corrupted key file '{}': expected 32 bytes, got {}",
                path.display(),
                bytes.len()
            ));
        }
        let secret: [u8; 32] = bytes.try_into().unwrap();
        let signing_key = SigningKey::from_bytes(&secret);
        let verifying_key = signing_key.verifying_key();
        tracing::info!(
            pubkey = %pubkey_to_hex(&verifying_key),
            path = %path.display(),
            "Loaded existing keypair"
        );
        Ok((signing_key, verifying_key))
    } else {
        let (signing_key, verifying_key) = generate_keypair();
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create key directory: {}", e))?;
        }
        std::fs::write(path, signing_key.to_bytes())
            .map_err(|e| format!("Failed to write key file '{}': {}", path.display(), e))?;
        tracing::info!(
            pubkey = %pubkey_to_hex(&verifying_key),
            path = %path.display(),
            "Generated and saved new keypair"
        );
        Ok((signing_key, verifying_key))
    }
}

// ─── Signing & Verification ────────────────────────────────────────────────────

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

    if sig_bytes.len() != 64 {
        return false;
    }

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

// ─── Receipt Signing (Deterministic Binary Format) ─────────────────────────────
//
// Uses canonical little-endian binary encoding for ALL fields.
// This eliminates platform-dependent float formatting issues.

pub fn get_receipt_signing_bytes(receipt: &InferenceReceipt) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(256);
    // String fields: length-prefixed UTF-8
    append_str(&mut bytes, &receipt.receipt_id);
    append_str(&mut bytes, &receipt.client_pubkey);
    append_str(&mut bytes, &receipt.node_pubkey);
    append_str(&mut bytes, &receipt.query_hash);
    // Integer fields: little-endian fixed-width
    bytes.extend_from_slice(&(receipt.tokens_generated as u64).to_le_bytes());
    // Float fields: canonical IEEE 754 bits as u64 LE
    bytes.extend_from_slice(&receipt.tps.to_bits().to_le_bytes());
    bytes.extend_from_slice(&receipt.network_median_tps.to_bits().to_le_bytes());
    // Financial fields: exact integers
    bytes.extend_from_slice(&receipt.tqw_nano.to_le_bytes());
    bytes.extend_from_slice(&receipt.amount_nano.to_le_bytes());
    bytes.extend_from_slice(&receipt.timestamp.to_le_bytes());
    bytes
}

fn append_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
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

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keypair_roundtrip() {
        let (_, pub_key) = generate_keypair();
        let hex = pubkey_to_hex(&pub_key);
        let recovered = hex_to_pubkey(&hex).unwrap();
        assert_eq!(pub_key, recovered);
    }

    #[test]
    fn test_sign_verify_message() {
        let (priv_key, pub_key) = generate_keypair();
        let msg = b"hello iToken";
        let sig = sign_bytes(&priv_key, msg);
        assert!(verify_signature(&pubkey_to_hex(&pub_key), msg, &sig));
    }

    #[test]
    fn test_tampered_message_fails() {
        let (priv_key, pub_key) = generate_keypair();
        let msg = b"hello iToken";
        let sig = sign_bytes(&priv_key, msg);
        assert!(!verify_signature(&pubkey_to_hex(&pub_key), b"tampered", &sig));
    }

    #[test]
    fn test_receipt_sign_and_verify() {
        let (client_priv, client_pub) = generate_keypair();
        let (node_priv, node_pub) = generate_keypair();

        let mut receipt = InferenceReceipt {
            receipt_id: "test-123".into(),
            client_pubkey: pubkey_to_hex(&client_pub),
            node_pubkey: pubkey_to_hex(&node_pub),
            query_hash: sha256_hash("test query"),
            tokens_generated: 100,
            tps: 45.5,
            network_median_tps: 25.0,
            tqw_nano: 10_000_000,
            amount_nano: 500_000_000,
            timestamp: 1700000000,
            node_signature: None,
            client_signature: None,
        };

        sign_receipt_as_node(&node_priv, &mut receipt);
        sign_receipt_as_client(&client_priv, &mut receipt);

        assert!(verify_receipt_signatures(&receipt));
    }

    #[test]
    fn test_tampered_receipt_fails() {
        let (client_priv, client_pub) = generate_keypair();
        let (node_priv, node_pub) = generate_keypair();

        let mut receipt = InferenceReceipt {
            receipt_id: "test-456".into(),
            client_pubkey: pubkey_to_hex(&client_pub),
            node_pubkey: pubkey_to_hex(&node_pub),
            query_hash: sha256_hash("test"),
            tokens_generated: 50,
            tps: 30.0,
            network_median_tps: 25.0,
            tqw_nano: 10_000_000,
            amount_nano: 300_000_000,
            timestamp: 1700000000,
            node_signature: None,
            client_signature: None,
        };

        sign_receipt_as_node(&node_priv, &mut receipt);
        sign_receipt_as_client(&client_priv, &mut receipt);

        // Tamper with amount after signing
        receipt.amount_nano = 999_999_999;
        assert!(!verify_receipt_signatures(&receipt));
    }

    #[test]
    fn test_signing_bytes_deterministic() {
        let receipt = InferenceReceipt {
            receipt_id: "det-test".into(),
            client_pubkey: "aabbcc".into(),
            node_pubkey: "ddeeff".into(),
            query_hash: "hash123".into(),
            tokens_generated: 42,
            tps: 33.33,
            network_median_tps: 25.0,
            tqw_nano: 10_000_000,
            amount_nano: 420_000_000,
            timestamp: 1700000000,
            node_signature: None,
            client_signature: None,
        };
        let b1 = get_receipt_signing_bytes(&receipt);
        let b2 = get_receipt_signing_bytes(&receipt);
        assert_eq!(b1, b2, "Signing bytes must be deterministic");
    }

    #[test]
    fn test_key_persistence() {
        let dir = std::env::temp_dir().join("itoken_test_keys");
        let key_path = dir.join("test_persist.key");
        // Clean up from any previous run
        let _ = std::fs::remove_file(&key_path);

        // First call: generates and saves
        let (_, pub1) = load_or_generate_keypair(&key_path).unwrap();
        // Second call: loads existing
        let (_, pub2) = load_or_generate_keypair(&key_path).unwrap();

        assert_eq!(pub1, pub2, "Key must persist across loads");

        // Cleanup
        let _ = std::fs::remove_file(&key_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn test_invalid_hex_pubkey() {
        assert!(hex_to_pubkey("not-valid-hex").is_err());
        assert!(hex_to_pubkey("aabb").is_err()); // Too short
    }

    #[test]
    fn test_sha256_hash_deterministic() {
        let h1 = sha256_hash("hello");
        let h2 = sha256_hash("hello");
        assert_eq!(h1, h2);
        assert_ne!(h1, sha256_hash("world"));
    }
}
