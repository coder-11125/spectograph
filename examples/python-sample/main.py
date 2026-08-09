# Main entrypoint.
from utils import helper, dead_function
from unused_dep import unused_thing


def main():
    x = helper(2)
    print(x)


if __name__ == "__main__":
    main()
