from stringutil import reverse, shout
from mathutil import square, is_positive


def test_reverse():
    assert reverse("abc") == "cba"


def test_shout():
    assert shout("hi") == "HI!"


def test_square():
    assert square(4) == 16
    assert square(0) == 0


def test_is_positive():
    assert is_positive(3) is True
    assert is_positive(-1) is False
