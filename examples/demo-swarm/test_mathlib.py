from mathlib import is_even, double


def test_is_even():
    assert is_even(4) is True
    assert is_even(3) is False


def test_double():
    assert double(5) == 10
    assert double(0) == 0
