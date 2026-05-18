from auth import authenticate, check_credentials

def test_authenticate_success():
    user = authenticate("admin", "secret")
    assert user is not None
    assert user.name == "admin"

def test_authenticate_failure():
    user = authenticate("admin", "wrong")
    assert user is None

def test_check_credentials():
    assert check_credentials("admin", "secret") is True
    assert check_credentials("admin", "wrong") is False
