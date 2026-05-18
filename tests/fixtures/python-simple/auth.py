from models import User

def authenticate(username, password):
    if check_credentials(username, password):
        return User(username)
    return None

def check_credentials(username, password):
    return username == "admin" and password == "secret"
