import importlib.util
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).with_name("assemble-render-scheduling-report.py")
SPEC = importlib.util.spec_from_file_location("assemble_render_scheduling_report", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
REPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPORT)


def direct_scenario(final_intervals, active_intervals=None):
    graphics = {
        "render_scheduler": {"catch_up_bursts": 0},
        "measured_frame_intervals_ns": final_intervals,
        "frame_interval_p95_ns": sorted(final_intervals)[
            (len(final_intervals) - 1) * 95 // 100
        ],
        "frame_interval_p99_ns": sorted(final_intervals)[
            (len(final_intervals) - 1) * 99 // 100
        ],
        "warmup_opportunities": 60,
        "measured_opportunities": len(final_intervals),
        "measured_deadline_misses": 1,
        "measured_target_misses": 0,
    }
    result = {"scenario": "direct_120hz", "graphics": graphics}
    if active_intervals is not None:
        result["active_measurement"] = {
            "frame_intervals_ns": active_intervals,
            "opportunities": len(active_intervals),
            "deadline_misses": 1,
            "target_misses": 0,
        }
    return result


class DirectCadenceBoundaryTests(unittest.TestCase):
    def test_explicit_active_snapshot_excludes_idle_control_suffix(self):
        active = [8_000_000, 9_000_000, 40_000_000]
        final = active + [500_000_000, 5_500_000_000]

        gate = REPORT.scenario_gate(direct_scenario(final, active), direct=True)

        self.assertEqual(gate["active_presented_frame_intervals"], 3)
        self.assertEqual(gate["presented_frame_intervals"]["samples"], 3)
        self.assertEqual(gate["render_hint_deadline_misses"], 1)
        self.assertEqual(
            gate["frames_per_second_milli"],
            3 * 1_000_000_000_000 // sum(active),
        )

    def test_missing_active_boundary_is_rejected(self):
        with self.assertRaisesRegex(REPORT.EvidenceError, "active_measurement"):
            REPORT.scenario_gate(direct_scenario([8_000_000]), direct=True)

    def test_active_snapshot_must_be_chronological_prefix(self):
        with self.assertRaisesRegex(REPORT.EvidenceError, "not a prefix"):
            REPORT.scenario_gate(
                direct_scenario([8_000_000, 9_000_000], [9_000_000]), direct=True
            )

    def test_active_counter_cannot_exceed_final_counter(self):
        scenario = direct_scenario([8_000_000], [8_000_000])
        scenario["active_measurement"]["deadline_misses"] = 2
        with self.assertRaisesRegex(REPORT.EvidenceError, "exceeds final"):
            REPORT.scenario_gate(scenario, direct=True)


if __name__ == "__main__":
    unittest.main()
