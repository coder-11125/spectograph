class Base:
    def greet(self):
        return "base"

    def base_only(self):
        return "base only"


class Child(Base):
    def greet(self):
        return "child " + super().greet()

    def unused_child_method(self):
        return "never called"


def main():
    c = Child()
    print(c.greet())


if __name__ == "__main__":
    main()
