import functools
import time
import hmac
import hashlib
import json
import base64
from flask import request, jsonify, g

SECRET_KEY = "dev-secret-change-in-production"
TOKEN_TTL = 3600


def create_token(user_id: int, role: str) -> str:
    header = base64.urlsafe_b64encode(json.dumps({"alg": "HS256"}).encode()).decode()
    payload_data = {
        "sub": user_id,
        "role": role,
        "iat": int(time.time()),
        "exp": int(time.time()) + TOKEN_TTL,
    }
    payload = base64.urlsafe_b64encode(json.dumps(payload_data).encode()).decode()
    signature = hmac.new(
        SECRET_KEY.encode(), f"{header}.{payload}".encode(), hashlib.sha256
    ).hexdigest()
    return f"{header}.{payload}.{signature}"


def decode_token(token: str) -> dict | None:
    parts = token.split(".")
    if len(parts) != 3:
        return None
    header, payload, sig = parts
    expected_sig = hmac.new(
        SECRET_KEY.encode(), f"{header}.{payload}".encode(), hashlib.sha256
    ).hexdigest()
    if not hmac.compare_digest(sig, expected_sig):
        return None
    try:
        data = json.loads(base64.urlsafe_b64decode(payload + "=="))
    except (json.JSONDecodeError, ValueError):
        return None
    if data.get("exp", 0) < time.time():
        return None
    return data


def require_auth(fn):
    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        auth_header = request.headers.get("Authorization", "")
        if not auth_header.startswith("Bearer "):
            return jsonify({"error": "Missing authorization"}), 401
        token = auth_header[7:]
        claims = decode_token(token)
        if claims is None:
            return jsonify({"error": "Invalid or expired token"}), 401
        g.user_id = claims["sub"]
        g.user_role = claims["role"]
        return fn(*args, **kwargs)
    return wrapper


def require_role(role: str):
    def decorator(fn):
        @functools.wraps(fn)
        def wrapper(*args, **kwargs):
            if getattr(g, "user_role", None) != role:
                return jsonify({"error": "Forbidden"}), 403
            return fn(*args, **kwargs)
        return wrapper
    return decorator
