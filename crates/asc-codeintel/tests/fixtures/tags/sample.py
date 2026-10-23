LIMIT = 3


class Greeter:
    """Greets people."""

    prefix = "hi"

    def greet(self, name):
        """Say hello."""
        return self.fmt(name)

    @staticmethod
    def fmt(name):
        return helper(name)


def helper(s):
    # not a docstring
    return s.strip()
