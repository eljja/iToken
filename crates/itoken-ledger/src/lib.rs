use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use itoken_core::types::{InferenceReceipt, format_itokens, MAX_RECEIPT_AGE_SECS, MAX_FUTURE_DRIFT_SECS};
use itoken_core::crypto::verify_receipt_signatures;

// ─── Ledger State ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct LedgerState {
    /// Balances in nano-iTokens (1 iToken = 1,000,000,000 nano)
    pub balances: HashMap<String, u64>,
    /// Set of receipt IDs already claimed (prevents double-spend)
    pub claimed_receipts: HashSet<String>,
}

// ─── Local Ledger ──────────────────────────────────────────────────────────────

pub struct LocalLedger {
    file_path: String,
    state: Mutex<LedgerState>,
}

impl LocalLedger {
    pub fn new(file_path: &str) -> Result<Self, String> {
        let path = Path::new(file_path);
        let state = if path.exists() {
            let mut file = File::open(path)
                .map_err(|e| format!("Failed to open ledger file '{}': {}", file_path, e))?;
            let mut content = String::new();
            file.read_to_string(&mut content)
                .map_err(|e| format!("Failed to read ledger file '{}': {}", file_path, e))?;
            serde_json::from_str(&content).map_err(|e| {
                error!(path = file_path, error = %e, "Corrupted ledger file, starting fresh");
                format!("Corrupted ledger file: {}", e)
            }).unwrap_or_else(|_| LedgerState {
                balances: HashMap::new(),
                claimed_receipts: HashSet::new(),
            })
        } else {
            LedgerState {
                balances: HashMap::new(),
                claimed_receipts: HashSet::new(),
            }
        };

        info!(path = file_path, accounts = state.balances.len(), "Ledger initialized");

        Ok(Self {
            file_path: file_path.to_string(),
            state: Mutex::new(state),
        })
    }

    /// Register a new account with an initial balance (nano-iTokens).
    /// If the account already exists, this is a no-op.
    pub fn register_account(&self, pubkey: &str, initial_nano: u64) {
        let mut state = self.state.lock();
        state.balances.entry(pubkey.to_string()).or_insert_with(|| {
            info!(
                pubkey = pubkey,
                balance = %format_itokens(initial_nano),
                "Account registered"
            );
            initial_nano
        });
        self.save_atomic(&state);
    }

    /// Get the balance of an account in nano-iTokens.
    pub fn get_balance(&self, pubkey: &str) -> u64 {
        let state = self.state.lock();
        state.balances.get(pubkey).copied().unwrap_or(0)
    }

    /// Transfer nano-iTokens between accounts. Uses checked arithmetic to prevent overflow.
    pub fn transfer(&self, from: &str, to: &str, amount_nano: u64) -> Result<(), String> {
        if amount_nano == 0 {
            return Err("Transfer amount must be positive".to_string());
        }

        let mut state = self.state.lock();

        let from_bal = state.balances.get(from).copied().unwrap_or(0);
        if from_bal < amount_nano {
            return Err(format!(
                "Insufficient balance: have {} iTokens, need {}",
                format_itokens(from_bal),
                format_itokens(amount_nano)
            ));
        }

        let to_bal = state.balances.get(to).copied().unwrap_or(0);
        let new_to_bal = to_bal.checked_add(amount_nano)
            .ok_or_else(|| "Receiver balance overflow".to_string())?;

        state.balances.insert(from.to_string(), from_bal - amount_nano);
        state.balances.insert(to.to_string(), new_to_bal);

        info!(
            from = from,
            to = to,
            amount = %format_itokens(amount_nano),
            from_balance = %format_itokens(from_bal - amount_nano),
            to_balance = %format_itokens(new_to_bal),
            "Transfer executed"
        );

        self.save_atomic(&state);
        Ok(())
    }

    /// Validate and claim an inference receipt, transferring iTokens from client to node.
    /// This performs 6 validation checks before executing the payment.
    pub fn claim_receipt(
        &self,
        receipt: &InferenceReceipt,
    ) -> Result<(), String> {
        let mut state = self.state.lock();

        // 1. Double-claim protection
        if state.claimed_receipts.contains(&receipt.receipt_id) {
            warn!(receipt_id = %receipt.receipt_id, "Double-claim attempt rejected");
            return Err("Receipt has already been claimed".to_string());
        }

        // 2. Timestamp validation — reject expired or future-dated receipts
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("System clock error: {}", e))?
            .as_secs();

        if receipt.timestamp + MAX_RECEIPT_AGE_SECS < now {
            warn!(
                receipt_id = %receipt.receipt_id,
                receipt_time = receipt.timestamp,
                now = now,
                "Expired receipt rejected"
            );
            return Err(format!(
                "Receipt has expired (age: {}s, max: {}s)",
                now - receipt.timestamp,
                MAX_RECEIPT_AGE_SECS
            ));
        }
        if receipt.timestamp > now + MAX_FUTURE_DRIFT_SECS {
            warn!(
                receipt_id = %receipt.receipt_id,
                receipt_time = receipt.timestamp,
                now = now,
                "Future-dated receipt rejected"
            );
            return Err("Receipt timestamp is in the future".to_string());
        }

        // 3. Cryptographic signature verification
        if !verify_receipt_signatures(receipt) {
            warn!(receipt_id = %receipt.receipt_id, "Invalid signatures rejected");
            return Err("Invalid receipt signatures".to_string());
        }

        // 4. Verify mathematical correctness — EXACT integer match, no tolerance
        let expected_amount = receipt.compute_amount();
        if receipt.amount_nano != expected_amount {
            warn!(
                receipt_id = %receipt.receipt_id,
                expected = expected_amount,
                actual = receipt.amount_nano,
                "Amount mismatch rejected"
            );
            return Err(format!(
                "Receipt amount mismatch: expected {} nano, got {} nano",
                expected_amount, receipt.amount_nano
            ));
        }

        // 5. Verify client has sufficient balance
        let client_bal = state.balances.get(&receipt.client_pubkey).copied().unwrap_or(0);
        if client_bal < receipt.amount_nano {
            return Err(format!(
                "Client has insufficient balance: {} < {}",
                format_itokens(client_bal),
                format_itokens(receipt.amount_nano)
            ));
        }

        // 6. Execute payment transfer (checked arithmetic)
        let node_bal = state.balances.get(&receipt.node_pubkey).copied().unwrap_or(0);
        let new_node_bal = node_bal.checked_add(receipt.amount_nano)
            .ok_or_else(|| "Node balance overflow".to_string())?;

        state.balances.insert(receipt.client_pubkey.clone(), client_bal - receipt.amount_nano);
        state.balances.insert(receipt.node_pubkey.clone(), new_node_bal);
        state.claimed_receipts.insert(receipt.receipt_id.clone());

        info!(
            receipt_id = %receipt.receipt_id,
            amount = %format_itokens(receipt.amount_nano),
            client = %receipt.client_pubkey,
            node = %receipt.node_pubkey,
            client_balance = %format_itokens(client_bal - receipt.amount_nano),
            node_balance = %format_itokens(new_node_bal),
            "Receipt claimed and payout executed"
        );

        self.save_atomic(&state);
        Ok(())
    }

    /// Claim a receipt received via Gossipsub sync.
    /// This only verifies cryptographic signatures, uniqueness (double-spend),
    /// and balance sufficiency. It does NOT verify the timestamp or the amount
    /// against local wall-clock time or local median TPS, preventing consensus splits.
    pub fn claim_gossip_receipt(&self, receipt: &InferenceReceipt) -> Result<(), String> {
        let mut state = self.state.lock();

        // 1. Double-claim protection
        if state.claimed_receipts.contains(&receipt.receipt_id) {
            return Err("Receipt has already been claimed".to_string());
        }

        // 2. Cryptographic signature verification
        if !verify_receipt_signatures(receipt) {
            return Err("Invalid receipt signatures".to_string());
        }

        // 3. Verify client has sufficient balance
        let client_bal = state.balances.get(&receipt.client_pubkey).copied().unwrap_or(0);
        if client_bal < receipt.amount_nano {
            return Err(format!(
                "Client has insufficient balance: {} < {}",
                format_itokens(client_bal),
                format_itokens(receipt.amount_nano)
            ));
        }

        // 4. Execute payment transfer (checked arithmetic)
        let node_bal = state.balances.get(&receipt.node_pubkey).copied().unwrap_or(0);
        let new_node_bal = node_bal.checked_add(receipt.amount_nano)
            .ok_or_else(|| "Node balance overflow".to_string())?;

        state.balances.insert(receipt.client_pubkey.clone(), client_bal - receipt.amount_nano);
        state.balances.insert(receipt.node_pubkey.clone(), new_node_bal);
        state.claimed_receipts.insert(receipt.receipt_id.clone());

        info!(
            receipt_id = %receipt.receipt_id,
            amount = %format_itokens(receipt.amount_nano),
            client = %receipt.client_pubkey,
            node = %receipt.node_pubkey,
            client_balance = %format_itokens(client_bal - receipt.amount_nano),
            node_balance = %format_itokens(new_node_bal),
            "Gossip receipt claimed and payout executed"
        );

        self.save_atomic(&state);
        Ok(())
    }

    /// Export all balances and claimed receipts (for P2P state sync).
    pub fn export_state(&self) -> (HashMap<String, u64>, HashSet<String>) {
        let state = self.state.lock();
        (state.balances.clone(), state.claimed_receipts.clone())
    }

    /// Import ledger state from an external source (synchronization).
    /// Merges missing claimed receipts and updates balances.
    pub fn import_state(
        &self,
        external_balances: HashMap<String, u64>,
        external_claimed_receipts: HashSet<String>,
    ) {
        let mut state = self.state.lock();

        let before_receipts = state.claimed_receipts.len();
        state.claimed_receipts.extend(external_claimed_receipts);
        let after_receipts = state.claimed_receipts.len();

        for (pubkey, ext_bal) in external_balances {
            let local_bal = state.balances.entry(pubkey).or_insert(0);
            *local_bal = ext_bal;
        }

        if after_receipts > before_receipts {
            info!(
                imported_receipts = after_receipts - before_receipts,
                "Ledger state merged successfully"
            );
            self.save_atomic(&state);
        }
    }

    /// Atomic file write: write to temp file, fsync, then rename.
    /// This prevents data corruption on crash.
    fn save_atomic(&self, state: &LedgerState) {
        let tmp_path = format!("{}.tmp", self.file_path);
        let serialized = match serde_json::to_string_pretty(state) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "Failed to serialize ledger state");
                return;
            }
        };

        let result = (|| -> std::io::Result<()> {
            let mut file = File::create(&tmp_path)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &self.file_path)?;
            Ok(())
        })();

        if let Err(e) = result {
            error!(error = %e, path = %self.file_path, "CRITICAL: Failed to persist ledger state");
            // Attempt cleanup of temp file
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use itoken_core::types::NANO_PER_ITOKEN;

    fn temp_ledger() -> (LocalLedger, String) {
        let path = format!(
            "{}/itoken_test_ledger_{}.json",
            std::env::temp_dir().display(),
            uuid_v4_simple()
        );
        let ledger = LocalLedger::new(&path).unwrap();
        (ledger, path)
    }

    fn uuid_v4_simple() -> String {
        use rand::RngCore;
        let mut buf = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut buf);
        hex::encode(buf)
    }

    #[test]
    fn test_register_and_balance() {
        let (ledger, path) = temp_ledger();
        ledger.register_account("alice", 100 * NANO_PER_ITOKEN);
        assert_eq!(ledger.get_balance("alice"), 100 * NANO_PER_ITOKEN);
        assert_eq!(ledger.get_balance("unknown"), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_transfer_success() {
        let (ledger, path) = temp_ledger();
        ledger.register_account("alice", 100 * NANO_PER_ITOKEN);
        ledger.register_account("bob", 0);
        ledger.transfer("alice", "bob", 30 * NANO_PER_ITOKEN).unwrap();
        assert_eq!(ledger.get_balance("alice"), 70 * NANO_PER_ITOKEN);
        assert_eq!(ledger.get_balance("bob"), 30 * NANO_PER_ITOKEN);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_transfer_insufficient_balance() {
        let (ledger, path) = temp_ledger();
        ledger.register_account("alice", 10 * NANO_PER_ITOKEN);
        let result = ledger.transfer("alice", "bob", 50 * NANO_PER_ITOKEN);
        assert!(result.is_err());
        assert_eq!(ledger.get_balance("alice"), 10 * NANO_PER_ITOKEN);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_transfer_zero_rejected() {
        let (ledger, path) = temp_ledger();
        ledger.register_account("alice", 10 * NANO_PER_ITOKEN);
        let result = ledger.transfer("alice", "bob", 0);
        assert!(result.is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_atomic_save_creates_valid_json() {
        let (ledger, path) = temp_ledger();
        ledger.register_account("test", 42 * NANO_PER_ITOKEN);

        // Read back the file and verify it's valid JSON
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: LedgerState = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.balances.get("test"), Some(&(42 * NANO_PER_ITOKEN)));
        let _ = std::fs::remove_file(path);
    }
}
