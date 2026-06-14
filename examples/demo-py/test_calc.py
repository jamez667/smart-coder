from calc import is_even


def test_four_is_even():
    assert is_even(4) is True


def test_three_is_odd():
    assert is_even(3) is False


def test_ten_is_even():
    assert is_even(10) is True
