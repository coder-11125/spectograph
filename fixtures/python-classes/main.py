class Greeter:
    def __init__(self, name):
        self.name = name

    def greet(self):
        return self._make_greeting()

    def _make_greeting(self):
        return "Hello " + self.name

    def unused_method(self):
        return "never called"


def main():
    g = Greeter("world")
    print(g.greet())


if __name__ == "__main__":
    main()
