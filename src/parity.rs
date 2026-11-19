//! Checks against vectors the Python version made (tests/vectors): the same windows, CME stretches, title checks, signatures,
//! amounts and parsers, byte for byte.
#![cfg(test)]
use crate::cards::{XAU, window_title_text};
use crate::clock::cme_shuts;
use crate::rules::{RULES, windows};
use serde_json::Value;

pub fn vectors(name: &str) -> Value {
    let path = format!("{}/tests/vectors/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

#[test]
fn windows_match_python() {
    let v = vectors("windows.json");
    let (from, to) = (v["from"].as_i64().unwrap(), v["to"].as_i64().unwrap());
    for rule in RULES.iter() {
        let want: Vec<i64> = v["windows"][rule.key].as_array().unwrap().iter().map(|x| x.as_i64().unwrap()).collect();
        assert_eq!(windows(rule, from, to), want, "{}", rule.key);
    }
    let shuts: Vec<(i64, i64)> =
        v["shuts"].as_array().unwrap().iter().map(|p| (p[0].as_i64().unwrap(), p[1].as_i64().unwrap())).collect();
    assert_eq!(cme_shuts(from, from + 40 * 86_400_000), shuts);
    for row in v["titles"].as_array().unwrap() {
        assert_eq!(window_title_text(row[0].as_str().unwrap(), row[1].as_i64().unwrap()), row[2].as_str().unwrap(), "{row}");
    }
    for row in v["names"].as_array().unwrap() {
        assert_eq!(XAU.name_of(row[0].as_i64().unwrap()), row[1].as_str().unwrap());
    }
}

#[test]
fn signatures_match_python() {
    use crate::wallet::{Wallet, build_order, limit_amounts, order_body, to_wei};
    let v = vectors("wallet.json");
    for w in v["wallets"].as_array().unwrap() {
        let wallet = Wallet::new(w["key"].as_str().unwrap(), w["chain"].as_i64().unwrap(), w["account"].as_str().unwrap()).unwrap();
        assert_eq!(wallet.signer, w["signer"].as_str().unwrap());
        assert_eq!(wallet.maker, w["maker"].as_str().unwrap());
        assert_eq!(wallet.sign_text(w["message"].as_str().unwrap()), w["text_sig"].as_str().unwrap());
        for o in w["orders"].as_array().unwrap() {
            let a = limit_amounts(true, to_wei(o["price"].as_f64().unwrap()), to_wei(o["shares"].as_f64().unwrap())).unwrap();
            assert_eq!(a.maker.to_string(), o["maker"].as_str().unwrap());
            assert_eq!(a.taker.to_string(), o["taker"].as_str().unwrap());
            assert_eq!(a.price_per_share.to_string(), o["pps"].as_str().unwrap());
            let order = build_order(&wallet, o["token"].as_str().unwrap(), a.maker, a.taker, o["fee"].as_i64().unwrap(),
                                    o["exp"].as_i64().unwrap(), o["salt"].as_u64().unwrap());
            let (hash, sig) = wallet.sign_order(&order, o["neg"].as_bool().unwrap(), o["yb"].as_bool().unwrap()).unwrap();
            assert_eq!(hash, o["digest"].as_str().unwrap());
            assert_eq!(sig, o["sig"].as_str().unwrap());
            let body = order_body(&order, &sig, &hash, a.price_per_share);
            assert_eq!(String::from_utf8(body).unwrap(), o["body"].as_str().unwrap());
        }
    }
    for row in v["amounts"].as_array().unwrap() {
        let got = limit_amounts(true, to_wei(row["price"].as_f64().unwrap()), to_wei(row["qty"].as_f64().unwrap()));
        match (got, row["res"].get("error")) {
            (Err(e), Some(want)) => assert_eq!(e, want.as_str().unwrap()),
            (Ok(a), None) => {
                assert_eq!(a.maker.to_string(), row["res"]["maker"].as_str().unwrap(), "{row}");
                assert_eq!(a.taker.to_string(), row["res"]["taker"].as_str().unwrap(), "{row}");
                assert_eq!(a.amount.to_string(), row["res"]["amount"].as_str().unwrap(), "{row}");
                assert_eq!(a.price_per_share.to_string(), row["res"]["price_per_share"].as_str().unwrap(), "{row}");
            }
            (got, want) => panic!("{row}: {got:?} vs {want:?}"),
        }
    }
    for row in v["to_wei"].as_array().unwrap() {
        assert_eq!(to_wei(row[0].as_f64().unwrap()).to_string(), row[1].as_str().unwrap(), "{row}");
    }
}
