class User:
    def __init__(self, name):
        self.name = name

    def display(self):
        return f"User({self.name})"

class AdminUser(User):
    def __init__(self, name):
        super().__init__(name)
        self.role = "admin"

    def display(self):
        return f"AdminUser({self.name})"
