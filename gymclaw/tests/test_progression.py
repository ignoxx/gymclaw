import pytest

from gymclaw.services.progression import PerformanceSet, double_progression


@pytest.mark.parametrize("reps,weights,progressed", [([10, 10, 10], [80, 80, 80], True), ([10, 9, 10], [80, 80, 80], False), ([10, 10], [80, 80], False), ([10, 10, 10], [80, 75, 80], False), ([12, 11, 10], [80, 80, 80], True)])
def test_double_progression(reps, weights, progressed):
    result = double_progression("bench", target_weight=80, rep_max=10, required_sets=3, increment=2.5, performance=tuple(PerformanceSet(weight=w, reps=r) for w, r in zip(weights, reps)))
    assert result.progressed == progressed
    assert result.next_weight == (82.5 if progressed else 80)


def test_decimal_increment():
    result = double_progression("x", target_weight=0.2, rep_max=10, required_sets=1, increment=0.1, performance=(PerformanceSet(weight=0.2, reps=10),))
    assert result.next_weight == 0.3
