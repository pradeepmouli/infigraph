from dataclasses import dataclass, field
from typing import Optional
from datetime import datetime
import hashlib
import secrets


@dataclass
class User:
    id: int
    name: str
    email: str
    role: str = "member"
    password_hash: str = ""
    created_at: datetime = field(default_factory=datetime.utcnow)
    updated_at: datetime = field(default_factory=datetime.utcnow)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "email": self.email,
            "role": self.role,
            "created_at": self.created_at.isoformat(),
            "updated_at": self.updated_at.isoformat(),
        }

    def set_password(self, raw: str) -> None:
        salt = secrets.token_hex(16)
        hashed = hashlib.sha256(f"{salt}:{raw}".encode()).hexdigest()
        self.password_hash = f"{salt}${hashed}"

    def check_password(self, raw: str) -> bool:
        if "$" not in self.password_hash:
            return False
        salt, expected = self.password_hash.split("$", 1)
        hashed = hashlib.sha256(f"{salt}:{raw}".encode()).hexdigest()
        return hashed == expected


class UserStore:
    def __init__(self):
        self._users: dict[int, User] = {}
        self._next_id: int = 1

    def create_user(self, name: str, email: str, role: str = "member") -> User:
        user = User(id=self._next_id, name=name, email=email, role=role)
        self._users[user.id] = user
        self._next_id += 1
        return user

    def get_user(self, user_id: int) -> Optional[User]:
        return self._users.get(user_id)

    def update_user(self, user_id: int, data: dict) -> Optional[User]:
        user = self._users.get(user_id)
        if not user:
            return None
        if "name" in data:
            user.name = data["name"]
        if "email" in data:
            user.email = data["email"]
        if "role" in data:
            user.role = data["role"]
        user.updated_at = datetime.utcnow()
        return user

    def delete_user(self, user_id: int) -> bool:
        if user_id in self._users:
            del self._users[user_id]
            return True
        return False

    def list_users(self, page: int = 1, per_page: int = 20) -> list[User]:
        all_users = sorted(self._users.values(), key=lambda u: u.id)
        start = (page - 1) * per_page
        return all_users[start : start + per_page]

    def search(self, query: str, filters: dict = None) -> list[User]:
        results = []
        q = query.lower()
        for user in self._users.values():
            if q in user.name.lower() or q in user.email.lower():
                if filters and "role" in filters:
                    if user.role != filters["role"]:
                        continue
                results.append(user)
        return results

    def find_by_email(self, email: str) -> Optional[User]:
        for user in self._users.values():
            if user.email == email:
                return user
        return None

    def count(self) -> int:
        return len(self._users)
