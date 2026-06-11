use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Mutex;
use serde::{Deserialize, Serialize};
use dpu_core::types::InferenceReceipt;
use dpu_core::crypto::verify_receipt_signatures;

#[derive(Debug, Serialize, Deserialize)]
pub struct LedgerState {
    pub balances: HashMap<String, f64>,
    pub claimed_receipts: HashSet<String>,
}

pub struct LocalLedger {
    file_path: String,
    state: Mutex<LedgerState>,
}

impl LocalLedger {
    pub fn new(file_path: &str) -> Self {
        let path = Path::new(file_path);
        let state = if path.exists() {
            let mut file = File::open(path).expect("Failed to open ledger file");
            let mut content = String::new();
            file.read_to_string(&mut content).expect("Failed to read ledger file");
            serde_json::from_str(&content).unwrap_or_else(|_| LedgerState {
                balances: HashMap::new(),
                claimed_receipts: HashSet::new(),
            })
        } else {
            LedgerState {
                balances: HashMap::new(),
                claimed_receipts: HashSet::new(),
            }
        };

        Self {
            file_path: file_path.to_string(),
            state: Mutex::new(state),
        }
    }

    pub fn register_account(&self, pubkey: &str, initial_balance: f64) {
        let mut state = self.state.lock().unwrap();
        state.balances.entry(pubkey.to_string()).or_insert(initial_balance);
        self.save_locked(&state);
    }

    pub fn get_balance(&self, pubkey: &str) -> f64 {
        let state = self.state.lock().unwrap();
        *state.balances.get(pubkey).unwrap_or(&0.0)
    }

    pub fn transfer(&self, from: &str, to: &str, amount: f64) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        
        let from_bal = state.balances.get(from).copied().unwrap_or(0.0);
        if from_bal < amount {
            return Err("Insufficient iToken balance".to_string());
        }

        state.balances.insert(from.to_string(), from_bal - amount);
        let to_bal = state.balances.get(to).copied().unwrap_or(0.0);
        state.balances.insert(to.to_string(), to_bal + amount);

        self.save_locked(&state);
        Ok(())
    }

    pub fn claim_receipt(&self, receipt: &InferenceReceipt) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();

        // 1. Double claim protection
        if state.claimed_receipts.contains(&receipt.receipt_id) {
            return Err("Receipt has already been claimed".to_string());
        }

        // 2. Cryptographic signature check
        if !verify_receipt_signatures(receipt) {
            return Err("Invalid receipt signatures".to_string());
        }

        // 3. Verify math
        let expected_amount = receipt.compute_amount();
        // Allow a tiny rounding tolerance due to floats (e.g. 0.0001)
        if (receipt.amount_itokens - expected_amount).abs() > 1e-4 {
            return Err(format!(
                "Receipt amount mismatch. Expected: {}, Got: {}",
                expected_amount, receipt.amount_itokens
            ));
        }

        // 4. Verify client balance
        let client_bal = state.balances.get(&receipt.client_pubkey).copied().unwrap_or(0.0);
        if client_bal < receipt.amount_itokens {
            return Err("Client has insufficient balance to pay receipt".to_string());
        }

        // 5. Transfer funds
        state.balances.insert(receipt.client_pubkey.clone(), client_bal - receipt.amount_itokens);
        let node_bal = state.balances.get(&receipt.node_pubkey).copied().unwrap_or(0.0);
        state.balances.insert(receipt.node_pubkey.clone(), node_bal + receipt.amount_itokens);

        // 6. Record receipt ID
        state.claimed_receipts.insert(receipt.receipt_id.clone());

        self.save_locked(&state);
        Ok(())
    }

    fn save_locked(&self, state: &LedgerState) {
        let serialized = serde_json::to_string_pretty(state).expect("Failed to serialize ledger state");
        let mut file = File::create(&self.file_path).expect("Failed to write ledger file");
        file.write_all(serialized.as_bytes()).expect("Failed to write ledger file content");
    }
}
