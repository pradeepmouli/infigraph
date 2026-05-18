from auth import authenticate
from models import User

APP_NAME = "myapp"

def main():
    user = authenticate("admin", "secret")
    if user:
        greet(user)

def greet(user):
    print(f"Hello, {user.name}!")

if __name__ == "__main__":
    main()
