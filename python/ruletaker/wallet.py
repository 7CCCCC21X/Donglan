"""The signing key and the orders it signs (a port of huice live.py: Wallet, build_order, limit_amounts and the official SDK's
amount helpers).

Orders are EIP-712 "predict.fun CTF Exchange" v1 orders on BNB chain. A plain wallet signs the order's digest itself; a
Predict account (the smart wallet behind the website: the Privy wallet's exported key as PREDICT_PRIVATE_KEY and the deposit
address as PREDICT_ACCOUNT) signs it wrapped the way the Kernel contract verifies (the official SDKs' format).

Speed: the digest is built here from precomputed type hashes and domain separators (every field of an order is a 32-byte
word), not through eth-account's generic typed-data encoder; the result is the same digest and the same signature, byte for
byte (tests/test_wallet.py checks both against the generic path). With coincurve installed eth-keys signs in its native
secp256k1 (≈0.1–0.5 ms an order, where its pure-Python curve takes ≈3 ms)."""
from __future__ import annotations

import decimal
import random
from decimal import Decimal as D
from typing import Any

WEI = 10 ** 18
MIN_ORDER_USD = 1.0  # Predict refuses an order "with a value of" under 0.9 USD; a round dollar keeps clear of its rounding
MAX_SALT = 2_147_483_648
ZERO_ADDRESS = "0x" + "0" * 40
PROTOCOL_NAME, PROTOCOL_VERSION = "predict.fun CTF Exchange", "1"
KERNEL_NAME, KERNEL_VERSION = "Kernel", "0.3.1"
PRICE_TICK_WEI = 10 ** 16  # Predict prices carry at most two decimals (whole cents)

# Contract addresses per chain, as published in the official SDKs (@predictdotfun/sdk, predict-sdk).
ADDRESSES: dict[int, dict[str, str]] = {
    56: {"CTF_EXCHANGE": "0x8BC070BEdAB741406F4B1Eb65A72bee27894B689",
         "NEG_RISK_CTF_EXCHANGE": "0x365fb81bd4A24D6303cd2F19c349dE6894D8d58A",
         "YIELD_BEARING_CTF_EXCHANGE": "0x6bEb5a40C032AFc305961162d8204CDA16DECFa5",
         "YIELD_BEARING_NEG_RISK_CTF_EXCHANGE": "0x8A289d458f5a134bA40015085A8F50Ffb681B41d",
         "USDT": "0x55d398326f99059fF775485246999027B3197955",
         "ECDSA_VALIDATOR": "0x845ADb2C711129d4f3966735eD98a9F09fC4cE57"},
    97: {"CTF_EXCHANGE": "0x2A6413639BD3d73a20ed8C95F634Ce198ABbd2d7",
         "NEG_RISK_CTF_EXCHANGE": "0xd690b2bd441bE36431F6F6639D7Ad351e7B29680",
         "YIELD_BEARING_CTF_EXCHANGE": "0x8a6B4Fa700A1e310b106E7a48bAFa29111f66e89",
         "YIELD_BEARING_NEG_RISK_CTF_EXCHANGE": "0x95D5113bc50eD201e319101bbca3e0E250662fCC",
         "USDT": "0xB32171ecD878607FFc4F8FC0bCcE6852BB3149E0",
         "ECDSA_VALIDATOR": "0x845ADb2C711129d4f3966735eD98a9F09fC4cE57"},
}
ORDER_TYPES = [("salt", "uint256"), ("maker", "address"), ("signer", "address"), ("taker", "address"),
               ("tokenId", "uint256"), ("makerAmount", "uint256"), ("takerAmount", "uint256"), ("expiration", "uint256"),
               ("nonce", "uint256"), ("feeRateBps", "uint256"), ("side", "uint8"), ("signatureType", "uint8")]
DOMAIN_TYPES = [("name", "string"), ("version", "string"), ("chainId", "uint256"), ("verifyingContract", "address")]
ORDER_TYPE_TEXT = "Order(" + ",".join(f"{t} {n}" for n, t in ORDER_TYPES) + ")"
DOMAIN_TYPE_TEXT = "EIP712Domain(" + ",".join(f"{t} {n}" for n, t in DOMAIN_TYPES) + ")"


def exchange_key(neg_risk: bool, yield_bearing: bool) -> str:
    """The exchange that settles a market: standard or multi-outcome (neg-risk), each with a yield-bearing twin."""
    return ("YIELD_BEARING_" if yield_bearing else "") + ("NEG_RISK_CTF_EXCHANGE" if neg_risk else "CTF_EXCHANGE")


def to_wei(value: Any) -> int:
    """0.46 -> 460000000000000000 exactly (through Decimal, rounded down), as the SDK's floatToWei."""
    return int((D(str(value)) * WEI).quantize(D(1), rounding=decimal.ROUND_DOWN))


def from_wei(value: Any) -> float:
    return int(value) / WEI


def tick_price(price_wei: int) -> int:
    """A price on Predict's tick: rounded down to the cent, except a value a hair under a cent boundary, which is that cent."""
    ticks, rem = divmod(max(price_wei, 0), PRICE_TICK_WEI)
    if rem and PRICE_TICK_WEI - rem <= 10 ** 9:
        ticks += 1
    return ticks * PRICE_TICK_WEI


def retain_sig(num: int, digits: int) -> int:
    """Keep ``digits`` significant digits of an integer (the rest become zeros): prices keep 3, quantities 5."""
    if num == 0:
        return 0
    sign, num = (-1, -num) if num < 0 else (1, num)
    excess = len(str(num)) - digits
    if excess <= 0:
        return sign * num
    scale = 10 ** excess
    return sign * (num // scale) * scale


def limit_amounts(buy: bool, price_wei: int, qty_wei: int) -> dict:
    """A LIMIT order's amounts: BUY offers price × shares USDT for the shares, SELL the other way round."""
    if price_wei <= 0:
        raise ValueError("价格必须大于 0")
    price, qty = tick_price(price_wei), retain_sig(qty_wei, 5)
    if price <= 0:
        raise ValueError("价格低于 1¢")
    if qty < 10 ** 16:
        raise ValueError("数量不足 0.01 份")
    notional = price * qty // WEI
    return {"price_per_share": price, "maker": notional if buy else qty, "taker": qty if buy else notional, "amount": qty}


def _eth() -> Any:
    try:
        import eth_abi
        import eth_utils
        from eth_account import Account
        from eth_account.messages import _hash_eip191_message, encode_defunct, encode_typed_data
    except ImportError as error:  # pragma: no cover - depends on the environment
        raise RuntimeError("真实交易需要 eth-account 包：pip install -r requirements.txt") from error
    return Account, encode_defunct, encode_typed_data, _hash_eip191_message, eth_abi, eth_utils


def keccak(data: bytes) -> bytes:
    return _eth()[5].keccak(data)


def word(value: int) -> bytes:
    return int(value).to_bytes(32, "big")


def address_word(address: str) -> bytes:
    return bytes(12) + bytes.fromhex(address[2:])


def build_order(wallet: "Wallet", token_id: str, maker_amount: int, taker_amount: int, fee_bps: int, expiration: int,
                salt: int | None = None) -> dict:
    """An unsigned BUY order as the exchange reads it (every uint as a decimal string, as the SDKs send them)."""
    return {"salt": str(random.SystemRandom().randint(0, MAX_SALT) if salt is None else salt),
            "maker": wallet.maker, "signer": wallet.maker, "taker": ZERO_ADDRESS, "tokenId": str(token_id),
            "makerAmount": str(maker_amount), "takerAmount": str(taker_amount), "expiration": str(int(expiration)),
            "nonce": "0", "feeRateBps": str(int(fee_bps)), "side": 0, "signatureType": 0}


class Wallet:
    """The signing key, and the account the orders are made for: the key's own address, or a Predict account it controls."""

    def __init__(self, private_key: str, chain_id: int, predict_account: str = ""):
        Account, _, _, _, _, eth_utils = _eth()
        if chain_id not in ADDRESSES:
            raise ValueError(f"不支持的链 {chain_id}（56 = BNB 主网，97 = BNB 测试网）")
        self.chain_id, self.addresses = chain_id, ADDRESSES[chain_id]
        self.account = Account.from_key(private_key)
        self.signer: str = self.account.address
        self.predict_account = eth_utils.to_checksum_address(predict_account) if predict_account else ""
        self.maker: str = self.predict_account or self.signer
        self.order_type_hash = keccak(ORDER_TYPE_TEXT.encode())
        self.domains: dict[str, bytes] = {}  # exchange key -> its EIP-712 domain separator
        domain_type = keccak(DOMAIN_TYPE_TEXT.encode())
        for key in ("CTF_EXCHANGE", "NEG_RISK_CTF_EXCHANGE", "YIELD_BEARING_CTF_EXCHANGE", "YIELD_BEARING_NEG_RISK_CTF_EXCHANGE"):
            self.domains[key] = keccak(domain_type + keccak(PROTOCOL_NAME.encode()) + keccak(PROTOCOL_VERSION.encode())
                                       + word(chain_id) + address_word(self.addresses[key]))
        self.kernel_domain = b""
        if self.predict_account:
            self.kernel_domain = keccak(domain_type + keccak(KERNEL_NAME.encode()) + keccak(KERNEL_VERSION.encode())
                                        + word(chain_id) + address_word(self.predict_account))
        self.kernel_type = keccak(b"Kernel(bytes32 hash)")

    @property
    def kind(self) -> str:
        return "Predict 账户（智能钱包）" if self.predict_account else "普通钱包"

    # -- the order --
    def order_digest(self, order: dict, neg_risk: bool, yield_bearing: bool) -> bytes:
        """keccak(0x19 0x01 ‖ the exchange's domain separator ‖ the order's struct hash)."""
        fields = b"".join(address_word(str(order[name])) if kind == "address" else word(int(order[name])) for name, kind in ORDER_TYPES)
        struct = keccak(self.order_type_hash + fields)
        return keccak(b"\x19\x01" + self.domains[exchange_key(neg_risk, yield_bearing)] + struct)

    def order_hash(self, order: dict, neg_risk: bool, yield_bearing: bool) -> str:
        return "0x" + self.order_digest(order, neg_risk, yield_bearing).hex()

    def sign_digest(self, digest: bytes) -> str:
        """The key's signature of a 32-byte digest, r ‖ s ‖ v (v 27 / 28), as eth-account signs a typed message."""
        return "0x" + bytes(self.account.unsafe_sign_hash(digest).signature).hex()

    def sign_order(self, order: dict, neg_risk: bool, yield_bearing: bool) -> tuple[str, str]:
        """(order hash, signature) for ``order`` on the exchange that settles its market."""
        digest = self.order_digest(order, neg_risk, yield_bearing)
        if self.predict_account:
            return "0x" + digest.hex(), self.kernel_sign(digest)
        return "0x" + digest.hex(), self.sign_digest(digest)

    def kernel_sign(self, message_hash: bytes) -> str:
        """A Predict account's signature of ``message_hash``: the hash wrapped in the Kernel domain of the account, signed as a
        personal message, prefixed with 0x01 and the ECDSA validator (the official SDKs' format)."""
        wrapped = keccak(self.kernel_type + bytes(message_hash))
        digest = keccak(b"\x19\x01" + self.kernel_domain + wrapped)
        personal = keccak(b"\x19Ethereum Signed Message:\n32" + digest)
        return "0x01" + self.addresses["ECDSA_VALIDATOR"][2:] + bytes(self.account.unsafe_sign_hash(personal).signature).hex()

    # -- the generic path (the sign-in message; and the tests' check of the fast one) --
    def typed_data(self, order: dict, neg_risk: bool, yield_bearing: bool) -> dict:
        return {"types": {"EIP712Domain": [{"name": n, "type": t} for n, t in DOMAIN_TYPES],
                          "Order": [{"name": n, "type": t} for n, t in ORDER_TYPES]},
                "primaryType": "Order",
                "domain": {"name": PROTOCOL_NAME, "version": PROTOCOL_VERSION, "chainId": self.chain_id,
                           "verifyingContract": self.addresses[exchange_key(neg_risk, yield_bearing)]},
                "message": {name: order[name] for name, _ in ORDER_TYPES}}

    def sign_order_generic(self, order: dict, neg_risk: bool, yield_bearing: bool) -> tuple[str, str]:
        """huice live.Wallet.sign_order, through eth-account's typed-data encoder: the reference the fast path is checked against."""
        _, encode_defunct, encode_typed_data, hash_eip191, eth_abi, eth_utils = _eth()
        message = encode_typed_data(full_message=self.typed_data(order, neg_risk, yield_bearing))
        digest = "0x" + bytes(hash_eip191(message)).hex()
        if self.predict_account:
            k = eth_utils.keccak
            domain = k(eth_abi.encode(["bytes32", "bytes32", "bytes32", "uint256", "address"],
                                      [k(text=DOMAIN_TYPE_TEXT), k(text=KERNEL_NAME), k(text=KERNEL_VERSION), self.chain_id, self.predict_account]))
            wrapped = k(eth_abi.encode(["bytes32", "bytes32"], [k(text="Kernel(bytes32 hash)"), bytes.fromhex(digest[2:])]))
            signed = self.account.sign_message(encode_defunct(primitive=k(b"\x19\x01" + domain + wrapped)))
            return digest, "0x01" + self.addresses["ECDSA_VALIDATOR"][2:] + bytes(signed.signature).hex()
        return digest, "0x" + bytes(self.account.sign_message(message).signature).hex()

    def sign_text(self, message: str) -> str:
        """The sign-in message (GET /v1/auth/message), EIP-191 signed; wrapped for a Predict account."""
        _, encode_defunct, _, hash_eip191, _, _ = _eth()
        if self.predict_account:
            return self.kernel_sign(bytes(hash_eip191(encode_defunct(text=message))))
        return "0x" + bytes(self.account.sign_message(encode_defunct(text=message)).signature).hex()


def native_signing() -> bool:
    """Whether eth-keys signs with coincurve's native secp256k1 (the fast path), not its own pure-Python curve."""
    try:
        from eth_keys.backends import get_backend
        return "coincurve" in type(get_backend()).__name__.lower()
    except Exception:
        return False
