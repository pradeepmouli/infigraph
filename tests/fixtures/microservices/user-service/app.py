from flask import Flask, request, jsonify, g
from models import User, UserStore
from auth import require_auth, create_token
import requests

app = Flask(__name__)
store = UserStore()

ORDER_SERVICE_URL = "http://order-service:3000"


@app.route("/api/users", methods=["GET"])
@require_auth
def list_users():
    page = request.args.get("page", 1, type=int)
    per_page = request.args.get("per_page", 20, type=int)
    users = store.list_users(page=page, per_page=per_page)
    return jsonify({
        "users": [u.to_dict() for u in users],
        "page": page,
        "per_page": per_page,
        "total": store.count(),
    })


@app.route("/api/users/<int:id>", methods=["GET", "PUT", "DELETE"])
@require_auth
def user_detail(id: int):
    if request.method == "GET":
        user = store.get_user(id)
        if not user:
            return jsonify({"error": "User not found"}), 404
        orders = fetch_user_orders(id)
        result = user.to_dict()
        result["orders"] = orders
        return jsonify(result)

    elif request.method == "PUT":
        data = request.get_json(force=True)
        user = store.update_user(id, data)
        if not user:
            return jsonify({"error": "User not found"}), 404
        return jsonify(user.to_dict())

    elif request.method == "DELETE":
        success = store.delete_user(id)
        if not success:
            return jsonify({"error": "User not found"}), 404
        cancel_user_orders(id)
        return jsonify({"deleted": True}), 200


@app.route("/api/users/search", methods=["POST"])
@require_auth
def search_users():
    body = request.get_json(force=True)
    query = body.get("query", "")
    filters = body.get("filters", {})
    results = store.search(query=query, filters=filters)
    return jsonify({"results": [u.to_dict() for u in results]})


@app.route("/api/users", methods=["POST"])
def create_user():
    data = request.get_json(force=True)
    if not data.get("email") or not data.get("name"):
        return jsonify({"error": "name and email required"}), 400
    user = store.create_user(
        name=data["name"],
        email=data["email"],
        role=data.get("role", "member"),
    )
    return jsonify(user.to_dict()), 201


@app.route("/api/auth/login", methods=["POST"])
def login():
    data = request.get_json(force=True)
    user = store.find_by_email(data.get("email", ""))
    if not user or not user.check_password(data.get("password", "")):
        return jsonify({"error": "Invalid credentials"}), 401
    token = create_token(user.id, user.role)
    return jsonify({"token": token, "user": user.to_dict()})


def fetch_user_orders(user_id: int) -> list:
    try:
        resp = requests.get(
            f"{ORDER_SERVICE_URL}/api/orders",
            params={"user_id": user_id},
            timeout=5,
        )
        resp.raise_for_status()
        return resp.json().get("orders", [])
    except requests.RequestException:
        return []


def cancel_user_orders(user_id: int) -> bool:
    try:
        resp = requests.post(
            f"{ORDER_SERVICE_URL}/api/orders/cancel-all",
            json={"user_id": user_id},
            timeout=5,
        )
        return resp.status_code == 200
    except requests.RequestException:
        return False


if __name__ == "__main__":
    app.run(host="0.0.0.0", port=5000, debug=True)
