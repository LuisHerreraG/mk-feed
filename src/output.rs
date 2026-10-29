//! Exit codes and the machine contract.
//!
//! The table is the contract an agent codes against: 0 ok, 1 error, 2 usage,
//! 3 refused, 4 partial (warnings, or an unknown publish).

use serde_json::Value;
use std::io::IsTerminal;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
#[allow(dead_code)]
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_REFUSED: i32 = 3;
pub const EXIT_PARTIAL: i32 = 4;

pub struct Ctx {
    pub json: bool,
    pub no_color: bool,
}

impl Ctx {
    pub fn new(json: bool, no_color: bool) -> Self {
        Self { json, no_color }
    }

    pub fn color(&self) -> bool {
        !self.no_color && std::io::stdout().is_terminal()
    }

    /// Print a document. Human mode prints `human`; JSON mode prints `doc`.
    pub fn emit(&self, doc: Value, human: impl FnOnce()) {
        if self.json {
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else {
            human();
        }
    }

    /// A refusal: the command understood the request and declined on purpose.
    pub fn refuse(&self, code: &str, message: &str, hint: Option<&str>) -> i32 {
        if self.json {
            let mut doc = serde_json::json!({
                "ok": false,
                "refused": true,
                "code": code,
                "error": message,
            });
            if let Some(hint) = hint {
                doc["hint"] = Value::String(hint.to_string());
            }
            println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        } else {
            eprintln!("refused: {message}");
            if let Some(hint) = hint {
                eprintln!("  hint: {hint}");
            }
        }
        EXIT_REFUSED
    }

    /// A hard error: something broke, not a decision.
    pub fn fail(&self, code: &str, message: &str) -> i32 {
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": false,
                    "refused": false,
                    "code": code,
                    "error": message,
                }))
                .unwrap_or_default()
            );
        } else {
            eprintln!("error: {message}");
        }
        EXIT_ERROR
    }
}
