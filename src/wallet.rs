//! The signing key and the orders it signs (Wallet, build_order, limit_amounts and the official SDK's amount helpers).
//!
//! Orders are EIP-712 "predict.fun CTF Exchange" v1 orders on BNB chain. A plain wallet signs the order's digest itself; a
//! Predict account (the smart wallet behind the website: the Privy wallet's exported key as PREDICT_PRIVATE_KEY and the deposit
//! address as PREDICT_ACCOUNT) signs it wrapped the way the Kernel contract verifies (the official SDKs' format). The digest is
//! built from precomputed type hashes and domain separators (every field of an order is a 32-byte word); signatures are
//! RFC 6979 deterministic with a low s, so the same order always gets the same signature (the Python version's, byte for byte).
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};
use sha3::{Digest, Keccak256};

pub const WEI: u128 = 1_000_000_000_000_000_000;
pub const MIN_ORDER_USD: f64 = 1.0;
pub const MAX_SALT: u64 = 2_147_483_648;
pub const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
pub const PROTOCOL_NAME: &str = "predict.fun CTF Exchange";
pub const PROTOCOL_VERSION: &str = "1";
pub const KERNEL_NAME: &str = "Kernel";
pub const KERNEL_VERSION: &str = "0.3.1";
pub const PRICE_TICK_WEI: u128 = 10_000_000_000_000_000;

pub const EXCHANGES: [&str; 4] =
    ["CTF_EXCHANGE", "NEG_RISK_CTF_EXCHANGE", "YIELD_BEARING_CTF_EXCHANGE", "YIELD_BEARING_NEG_RISK_CTF_EXCHANGE"];

/// Contract addresses per chain, as published in the official SDKs.
pub fn addresses(chain_id: i64) -> Option<[(&'static str, &'static str); 6]> {
    match chain_id {
        56 => Some([
            ("CTF_EXCHANGE", "0x8BC070BEdAB741406F4B1Eb65A72bee27894B689"),
            ("NEG_RISK_CTF_EXCHANGE", "0x365fb81bd4A24D6303cd2F19c349dE6894D8d58A"),
            ("YIELD_BEARING_CTF_EXCHANGE", "0x6bEb5a40C032AFc305961162d8204CDA16DECFa5"),
            ("YIELD_BEARING_NEG_RISK_CTF_EXCHANGE", "0x8A289d458f5a134bA40015085A8F50Ffb681B41d"),
            ("USDT", "0x55d398326f99059fF775485246999027B3197955"),
            ("ECDSA_VALIDATOR", "0x845ADb2C711129d4f3966735eD98a9F09fC4cE57"),
        ]),
        97 => Some([
            ("CTF_EXCHANGE", "0x2A6413639BD3d73a20ed8C95F634Ce198ABbd2d7"),
            ("NEG_RISK_CTF_EXCHANGE", "0xd690b2bd441bE36431F6F6639D7Ad351e7B29680"),
            ("YIELD_BEARING_CTF_EXCHANGE", "0x8a6B4Fa700A1e310b106E7a48bAFa29111f66e89"),
            ("YIELD_BEARING_NEG_RISK_CTF_EXCHANGE", "0x95D5113bc50eD201e319101bbca3e0E250662fCC"),
            ("USDT", "0xB32171ecD878607FFc4F8FC0bCcE6852BB3149E0"),
            ("ECDSA_VALIDATOR", "0x845ADb2C711129d4f3966735eD98a9F09fC4cE57"),
        ]),
        _ => None,
    }
}

pub const ORDER_TYPE_TEXT: &str = "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,\
uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)";
pub const DOMAIN_TYPE_TEXT: &str = "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";

pub fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// The exchange that settles a market: standard or multi-outcome (neg-risk), each with a yield-bearing twin.
pub fn exchange_key(neg_risk: bool, yield_bearing: bool) -> &'static str {
    match (yield_bearing, neg_risk) {
        (false, false) => "CTF_EXCHANGE",
        (false, true) => "NEG_RISK_CTF_EXCHANGE",
        (true, false) => "YIELD_BEARING_CTF_EXCHANGE",
        (true, true) => "YIELD_BEARING_NEG_RISK_CTF_EXCHANGE",
    }
}

/// A decimal text (digits, an optional point) -> its value × 10^18, rounded down; None for what is not such a number.
pub fn decimal_to_wei(text: &str) -> Option<u128> {
    let t = text.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = t.split_once('.').unwrap_or((t, ""));
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut value: u128 = 0;
    for b in int_part.bytes() {
        value = value.checked_mul(10)?.checked_add((b - b'0') as u128)?;
    }
    value = value.checked_mul(WEI)?;
    let mut scale = WEI / 10;
    for b in frac_part.bytes().take(18) {
        value = value.checked_add((b - b'0') as u128 * scale)?;
        scale /= 10;
    }
    if neg && value > 0 {
        return None;
    }
    Some(value)
}

/// 0.46 -> 460000000000000000 exactly (through the float's shortest decimal digits, rounded down), as the SDK's floatToWei.
pub fn to_wei(value: f64) -> u128 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    decimal_to_wei(&format!("{value}")).unwrap_or(0)
}

pub fn from_wei(value: u128) -> f64 {
    value as f64 / WEI as f64
}

/// A price on Predict's tick: rounded down to the cent, except a value a hair under a cent boundary, which is that cent.
pub fn tick_price(price_wei: u128) -> u128 {
    let (mut ticks, rem) = (price_wei / PRICE_TICK_WEI, price_wei % PRICE_TICK_WEI);
    if rem != 0 && PRICE_TICK_WEI - rem <= 1_000_000_000 {
        ticks += 1;
    }
    ticks * PRICE_TICK_WEI
}

/// Keep ``digits`` significant digits of an integer (the rest become zeros): prices keep 3, quantities 5.
pub fn retain_sig(num: u128, digits: u32) -> u128 {
    if num == 0 {
        return 0;
    }
    let len = num.to_string().len() as u32;
    if len <= digits {
        return num;
    }
    let scale = 10u128.pow(len - digits);
    num / scale * scale
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Amounts {
    pub price_per_share: u128,
    pub maker: u128,
    pub taker: u128,
    pub amount: u128,
}

/// A LIMIT order's amounts: BUY offers price × shares USDT for the shares, SELL the other way round.
pub fn limit_amounts(buy: bool, price_wei: u128, qty_wei: u128) -> Result<Amounts, String> {
    if price_wei == 0 {
        return Err("价格必须大于 0".into());
    }
    let (price, qty) = (tick_price(price_wei), retain_sig(qty_wei, 5));
    if price == 0 {
        return Err("价格低于 1¢".into());
    }
    if qty < 10_000_000_000_000_000 {
        return Err("数量不足 0.01 份".into());
    }
    // price is a whole number of cents: price × qty / 10^18 is (cents × qty) / 100 exactly, with no overflow
    let notional = (price / PRICE_TICK_WEI) * qty / 100;
    Ok(Amounts {
        price_per_share: price,
        maker: if buy { notional } else { qty },
        taker: if buy { qty } else { notional },
        amount: qty,
    })
}

/// A uint256 in decimal digits -> its 32-byte big-endian word.
pub fn uint_word(text: &str) -> Result<[u8; 32], String> {
    let t = text.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("不是整数：{t}"));
    }
    let mut out = [0u8; 32];
    for b in t.bytes() {
        let mut carry = (b - b'0') as u32;
        for byte in out.iter_mut().rev() {
            let v = *byte as u32 * 10 + carry;
            *byte = (v & 0xff) as u8;
            carry = v >> 8;
        }
        if carry != 0 {
            return Err("超过 uint256".into());
        }
    }
    Ok(out)
}

pub fn u128_word(v: u128) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&v.to_be_bytes());
    out
}

pub fn address_bytes(address: &str) -> Result<[u8; 20], String> {
    let hex_part = address.strip_prefix("0x").or_else(|| address.strip_prefix("0X")).unwrap_or(address);
    let raw = hex::decode(hex_part).map_err(|_| format!("地址格式不对：{address}"))?;
    raw.try_into().map_err(|_| format!("地址长度不对：{address}"))
}

pub fn address_word(address: &str) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(&address_bytes(address)?);
    Ok(out)
}

/// EIP-55 mixed-case checksum form of an address.
pub fn checksum(address: &[u8; 20]) -> String {
    let lower = hex::encode(address);
    let hash = keccak(lower.as_bytes());
    let mut out = String::from("0x");
    for (i, c) in lower.chars().enumerate() {
        let nibble = (hash[i / 2] >> if i % 2 == 0 { 4 } else { 0 }) & 0x0f;
        out.push(if c.is_ascii_alphabetic() && nibble >= 8 { c.to_ascii_uppercase() } else { c });
    }
    out
}

/// An unsigned BUY order as the exchange reads it (every uint as a decimal string, as the SDKs send them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: u128,
    pub taker_amount: u128,
    pub expiration: i64,
    pub nonce: u64,
    pub fee_rate_bps: i64,
    pub side: u8,
    pub signature_type: u8,
}

impl Order {
    /// The fields as the request carries them (the order of the type, every uint a decimal string, side and type numbers).
    pub fn fields(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("salt".into(), json!(self.salt.to_string()));
        m.insert("maker".into(), json!(self.maker));
        m.insert("signer".into(), json!(self.signer));
        m.insert("taker".into(), json!(self.taker));
        m.insert("tokenId".into(), json!(self.token_id));
        m.insert("makerAmount".into(), json!(self.maker_amount.to_string()));
        m.insert("takerAmount".into(), json!(self.taker_amount.to_string()));
        m.insert("expiration".into(), json!(self.expiration.to_string()));
        m.insert("nonce".into(), json!(self.nonce.to_string()));
        m.insert("feeRateBps".into(), json!(self.fee_rate_bps.to_string()));
        m.insert("side".into(), json!(self.side));
        m.insert("signatureType".into(), json!(self.signature_type));
        m
    }
}

pub fn build_order(
    wallet: &Wallet,
    token_id: &str,
    maker_amount: u128,
    taker_amount: u128,
    fee_bps: i64,
    expiration: i64,
    salt: u64,
) -> Order {
    Order {
        salt,
        maker: wallet.maker.clone(),
        signer: wallet.maker.clone(),
        taker: ZERO_ADDRESS.into(),
        token_id: token_id.to_string(),
        maker_amount,
        taker_amount,
        expiration,
        nonce: 0,
        fee_rate_bps: fee_bps,
        side: 0,
        signature_type: 0,
    }
}

/// The signing key, and the account the orders are made for: the key's own address, or a Predict account it controls.
pub struct Wallet {
    pub chain_id: i64,
    pub addresses: Vec<(&'static str, &'static str)>,
    key: SigningKey,
    pub signer: String,
    pub predict_account: String,
    pub maker: String,
    order_type_hash: [u8; 32],
    domains: Vec<(&'static str, [u8; 32])>,
    kernel_domain: [u8; 32],
    kernel_type: [u8; 32],
}

impl std::fmt::Debug for Wallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Wallet({}, maker {})", self.signer, self.maker)
    }
}

impl Wallet {
    pub fn new(private_key: &str, chain_id: i64, predict_account: &str) -> Result<Wallet, String> {
        let book = addresses(chain_id).ok_or_else(|| format!("不支持的链 {chain_id}（56 = BNB 主网，97 = BNB 测试网）"))?;
        let hex_key = private_key.trim().trim_start_matches("0x");
        let raw = hex::decode(hex_key).map_err(|_| "PREDICT_PRIVATE_KEY 应为 64 位十六进制（0x 可省）".to_string())?;
        let key = SigningKey::from_slice(&raw).map_err(|_| "PREDICT_PRIVATE_KEY 不是有效的私钥".to_string())?;
        let point = key.verifying_key().to_encoded_point(false);
        let digest = keccak(&point.as_bytes()[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&digest[12..]);
        let signer = checksum(&addr);
        let predict_account =
            if predict_account.trim().is_empty() { String::new() } else { checksum(&address_bytes(predict_account.trim())?) };
        let maker = if predict_account.is_empty() { signer.clone() } else { predict_account.clone() };
        let domain_type = keccak(DOMAIN_TYPE_TEXT.as_bytes());
        let domain = |name: &str, version: &str, contract: &str| -> Result<[u8; 32], String> {
            let mut buf = Vec::with_capacity(160);
            buf.extend_from_slice(&domain_type);
            buf.extend_from_slice(&keccak(name.as_bytes()));
            buf.extend_from_slice(&keccak(version.as_bytes()));
            buf.extend_from_slice(&u128_word(chain_id as u128));
            buf.extend_from_slice(&address_word(contract)?);
            Ok(keccak(&buf))
        };
        let mut domains = vec![];
        for name in EXCHANGES {
            let contract = book.iter().find(|(k, _)| *k == name).map(|(_, v)| *v).unwrap_or(ZERO_ADDRESS);
            domains.push((name, domain(PROTOCOL_NAME, PROTOCOL_VERSION, contract)?));
        }
        let kernel_domain =
            if predict_account.is_empty() { [0u8; 32] } else { domain(KERNEL_NAME, KERNEL_VERSION, &predict_account)? };
        // (the curve's precomputed tables made now, at the start, not at the first order)
        let _ = key.sign_prehash_recoverable(&[1u8; 32]);
        Ok(Wallet {
            chain_id,
            addresses: book.to_vec(),
            key,
            signer,
            predict_account,
            maker,
            order_type_hash: keccak(ORDER_TYPE_TEXT.as_bytes()),
            domains,
            kernel_domain,
            kernel_type: keccak(b"Kernel(bytes32 hash)"),
        })
    }

    pub fn address(&self, key: &str) -> &'static str {
        self.addresses.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(ZERO_ADDRESS)
    }

    pub fn kind(&self) -> &'static str {
        if self.predict_account.is_empty() { "普通钱包" } else { "Predict 账户（智能钱包）" }
    }

    /// keccak(0x19 0x01 ‖ the exchange's domain separator ‖ the order's struct hash).
    pub fn order_digest(&self, order: &Order, neg_risk: bool, yield_bearing: bool) -> Result<[u8; 32], String> {
        let mut buf = Vec::with_capacity(32 * 13);
        buf.extend_from_slice(&self.order_type_hash);
        buf.extend_from_slice(&u128_word(order.salt as u128));
        buf.extend_from_slice(&address_word(&order.maker)?);
        buf.extend_from_slice(&address_word(&order.signer)?);
        buf.extend_from_slice(&address_word(&order.taker)?);
        buf.extend_from_slice(&uint_word(&order.token_id)?);
        buf.extend_from_slice(&u128_word(order.maker_amount));
        buf.extend_from_slice(&u128_word(order.taker_amount));
        buf.extend_from_slice(&u128_word(order.expiration.max(0) as u128));
        buf.extend_from_slice(&u128_word(order.nonce as u128));
        buf.extend_from_slice(&u128_word(order.fee_rate_bps.max(0) as u128));
        buf.extend_from_slice(&u128_word(order.side as u128));
        buf.extend_from_slice(&u128_word(order.signature_type as u128));
        let structure = keccak(&buf);
        let key = exchange_key(neg_risk, yield_bearing);
        let domain = self.domains.iter().find(|(k, _)| *k == key).map(|(_, d)| *d).unwrap_or([0u8; 32]);
        let mut outer = Vec::with_capacity(66);
        outer.extend_from_slice(b"\x19\x01");
        outer.extend_from_slice(&domain);
        outer.extend_from_slice(&structure);
        Ok(keccak(&outer))
    }

    /// The key's signature of a 32-byte digest, r ‖ s ‖ v (v 27 / 28).
    pub fn sign_raw(&self, digest: &[u8; 32]) -> [u8; 65] {
        let (sig, recid) = self.key.sign_prehash_recoverable(digest).expect("a 32-byte digest always signs");
        let (sig, recid) = match sig.normalize_s() {
            Some(low) => (low, k256::ecdsa::RecoveryId::new(!recid.is_y_odd(), recid.is_x_reduced())),
            None => (sig, recid),
        };
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = 27 + recid.to_byte();
        out
    }

    pub fn sign_digest(&self, digest: &[u8; 32]) -> String {
        format!("0x{}", hex::encode(self.sign_raw(digest)))
    }

    /// (order hash, signature) for ``order`` on the exchange that settles its market.
    pub fn sign_order(&self, order: &Order, neg_risk: bool, yield_bearing: bool) -> Result<(String, String), String> {
        let digest = self.order_digest(order, neg_risk, yield_bearing)?;
        let hash = format!("0x{}", hex::encode(digest));
        Ok((hash, if self.predict_account.is_empty() { self.sign_digest(&digest) } else { self.kernel_sign(&digest) }))
    }

    /// A Predict account's signature of ``message_hash``: the hash wrapped in the Kernel domain of the account, signed as a
    /// personal message, prefixed with 0x01 and the ECDSA validator.
    pub fn kernel_sign(&self, message_hash: &[u8; 32]) -> String {
        let mut wrapped = Vec::with_capacity(64);
        wrapped.extend_from_slice(&self.kernel_type);
        wrapped.extend_from_slice(message_hash);
        let wrapped = keccak(&wrapped);
        let mut outer = Vec::with_capacity(66);
        outer.extend_from_slice(b"\x19\x01");
        outer.extend_from_slice(&self.kernel_domain);
        outer.extend_from_slice(&wrapped);
        let digest = keccak(&outer);
        let mut personal = Vec::with_capacity(60);
        personal.extend_from_slice(b"\x19Ethereum Signed Message:\n32");
        personal.extend_from_slice(&digest);
        let personal = keccak(&personal);
        let validator = self.address("ECDSA_VALIDATOR");
        format!("0x01{}{}", &validator[2..], hex::encode(self.sign_raw(&personal)))
    }

    /// The sign-in message (GET /v1/auth/message), EIP-191 signed; wrapped for a Predict account.
    pub fn sign_text(&self, message: &str) -> String {
        let mut buf = format!("\x19Ethereum Signed Message:\n{}", message.len()).into_bytes();
        buf.extend_from_slice(message.as_bytes());
        let hash = keccak(&buf);
        if self.predict_account.is_empty() { self.sign_digest(&hash) } else { self.kernel_sign(&hash) }
    }
}

/// The request body of an order: {"data": {"order": {fields, signature, hash}, "strategy": "LIMIT", "pricePerShare"}}.
pub fn order_body(order: &Order, signature: &str, hash: &str, price_per_share: u128) -> Vec<u8> {
    let quoted = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    format!(
        concat!(
            r#"{{"data":{{"order":{{"salt":"{}","maker":{},"signer":{},"taker":{},"tokenId":{},"makerAmount":"{}","takerAmount":"{}","#,
            r#""expiration":"{}","nonce":"{}","feeRateBps":"{}","side":{},"signatureType":{},"signature":{},"hash":{}}},"#,
            r#""strategy":"LIMIT","pricePerShare":"{}"}}}}"#
        ),
        order.salt,
        quoted(&order.maker),
        quoted(&order.signer),
        quoted(&order.taker),
        quoted(&order.token_id),
        order.maker_amount,
        order.taker_amount,
        order.expiration,
        order.nonce,
        order.fee_rate_bps,
        order.side,
        order.signature_type,
        quoted(signature),
        quoted(hash),
        price_per_share
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(to_wei(0.46), 460_000_000_000_000_000);
        assert_eq!(to_wei(0.58), 580_000_000_000_000_000);
        assert_eq!(to_wei(1e-5), 10_000_000_000_000);
        assert_eq!(tick_price(579_999_999_999_999_999), 580_000_000_000_000_000);
        assert_eq!(tick_price(585_000_000_000_000_000), 580_000_000_000_000_000);
        assert_eq!(retain_sig(123_456_789, 5), 123_450_000);
        let a = limit_amounts(true, to_wei(0.58), to_wei(900.0)).unwrap();
        assert_eq!(a.maker, 522 * WEI);
        assert_eq!(a.taker, 900 * WEI);
        assert!(limit_amounts(true, to_wei(0.58), to_wei(0.001)).is_err());
        assert_eq!(uint_word("256").unwrap()[30..], [1, 0]);
    }
}
