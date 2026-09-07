#!/usr/bin/env python3
"""Offline regression tests for paired evaluation accounting."""
import contextlib
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest

import harness_eval as ev


def spec():
    return {'schema': 'danso.eval.spec.v1', 'conditions': {
        'provider': 'openai', 'model': 'gpt-6-astra', 'effort': 'medium',
        'request_limit': 32, 'wall_seconds': 300, 'environment_sha256': 'a' * 64,
        'cache_policy': 'record observed cache; no guaranteed cold cache',
        'budget_enforcement': 'external recorder (must be implemented before execution)'},
        'harnesses': [{'id': name, 'revision': digit * 40, 'config_sha256': digit * 64}
                      for name, digit in [('danso', 'b'), ('pi', 'c')]],
        'cases': [{'id': name, 'prompt_sha256': 'd' * 64, 'input_sha256': 'e' * 64,
                   'acceptance_sha256': 'f' * 64} for name in ['edit', 'resume']],
        'repetitions': 2}


def receipt(plan, job, status='passed', tokens=100):
    return {'job': job['id'], 'plan_sha256': plan['sha256'], 'status': status,
            'exit_code': 0 if status == 'passed' else 1,
            'acceptance_passed': status == 'passed', 'evidence_sha256': '1' * 64,
            'metrics': {'elapsed_seconds': 10.5, 'requests': 3, 'total_tokens': tokens,
                        'compactions': 1, 'repeated_tools': 0}}


class Evaluation(unittest.TestCase):
    def test_plan_is_deterministic_counterbalanced_and_pins_conditions(self):
        source = spec()
        plan = ev.make_plan(source)
        self.assertEqual(plan, ev.make_plan(copy.deepcopy(source)))
        self.assertEqual([j['harness'] for j in plan['jobs']],
                         ['danso', 'pi', 'pi', 'danso', 'pi', 'danso', 'danso', 'pi'])
        source['conditions']['effort'] = 'high'
        self.assertNotEqual(ev.make_plan(source)['sha256'], plan['sha256'])

    def test_missing_runs_are_not_successes_or_zero_cost_observations(self):
        plan = ev.make_plan(spec())
        r = ev.summarize(plan, [receipt(plan, plan['jobs'][0])])
        self.assertFalse(r['complete'])
        self.assertEqual(len(r['missing_jobs']), 7)
        self.assertIsNone(r['harnesses']['pi']['pass_rate_submitted'])
        self.assertEqual(r['harnesses']['danso']['pass_rate_planned_lower_bound'], .25)
        self.assertIsNone(r['harnesses']['pi']['metrics_all_submitted']['total_tokens']['mean'])
        self.assertEqual(r['paired']['all_submitted_pairs']['pairs'], 0)

    def test_failures_stay_in_denominator_and_success_cohort_is_separate(self):
        plan = ev.make_plan(spec())
        rows = [receipt(plan, j, tokens=120 if j['harness'] == 'pi' else 100) for j in plan['jobs']]
        rows[1] = receipt(plan, plan['jobs'][1], status='timeout', tokens=500)
        rows[3] = receipt(plan, plan['jobs'][3], status='failed')
        r = ev.summarize(plan, rows)
        self.assertTrue(r['complete'])
        self.assertEqual(r['harnesses']['pi']['statuses']['timeout'], 1)
        self.assertEqual(r['harnesses']['pi']['pass_rate_submitted'], .75)
        self.assertEqual(r['paired']['all_submitted_pairs']['pairs'], 4)
        self.assertEqual(r['paired']['both_passed_pairs']['pairs'], 2)
        self.assertEqual(r['paired']['both_passed_pairs']['metric_deltas']['total_tokens']['mean'], 20)

    def test_unknown_metrics_are_not_imputed(self):
        plan = ev.make_plan(spec())
        rows = [receipt(plan, j) for j in plan['jobs']]
        rows[0]['metrics']['total_tokens'] = None
        result = ev.summarize(plan, rows)
        self.assertEqual(result['harnesses']['danso']['metrics_all_submitted']['total_tokens']['n'], 3)
        self.assertEqual(result['paired']['all_submitted_pairs']['metric_deltas']['total_tokens']['n'], 3)

    def test_budget_overruns_reported_even_for_passing_tasks(self):
        plan = ev.make_plan(spec())
        rows = [receipt(plan, j) for j in plan['jobs']]
        rows[0]['metrics']['requests'] = 33
        rows[1]['metrics']['elapsed_seconds'] = 301
        rows[2]['metrics']['requests'] = None
        r = ev.summarize(plan, rows)
        self.assertEqual(r['budget_violations'], [rows[0]['job'], rows[1]['job']])
        self.assertFalse(r['budget_observations_complete'])

    def test_duplicate_unknown_and_wrong_plan_receipts_rejected(self):
        plan = ev.make_plan(spec())
        row = receipt(plan, plan['jobs'][0])
        variants = [[row, row]]
        for key, value in [('job', 'unknown'), ('plan_sha256', '0' * 64)]:
            bad = copy.deepcopy(row); bad[key] = value; variants.append([bad])
        for rows in variants:
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                ev.summarize(plan, rows)

    def test_tampered_schedule_rejected(self):
        plan = ev.make_plan(spec()); plan['jobs'].reverse()
        with self.assertRaises(ValueError):
            ev.summarize(plan, [])

    def test_invalid_success_and_numeric_metrics_rejected(self):
        plan = ev.make_plan(spec())
        for key, value in [('acceptance_passed', False), ('exit_code', 1)]:
            row = receipt(plan, plan['jobs'][0]); row[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                ev.summarize(plan, [row])
        for value in [True, -1, float('nan'), float('inf'), '10', 1.5]:
            row = receipt(plan, plan['jobs'][0]); row['metrics']['requests'] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                ev.summarize(plan, [row])

    def test_invalid_spec_duplicates_and_limits_rejected(self):
        for key, value in [('repetitions', True), ('repetitions', 101), ('cases', []),
                           ('harnesses', [spec()['harnesses'][0]] * 2)]:
            source = spec(); source[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                ev.make_plan(source)
        source = spec(); source['cases'].append(source['cases'][0])
        with self.assertRaises(ValueError):
            ev.make_plan(source)

    def test_cli_roundtrip_and_body_free_error(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'spec.json'; path.write_text(json.dumps(spec()))
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                self.assertEqual(ev.main(['plan', str(path)]), 0)
            path.write_text(output.getvalue())
            rows = Path(root) / 'rows.json'; rows.write_text('[]')
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(ev.main(['report', str(path), str(rows)]), 0)
            rows.write_text('{"private-marker":')
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                self.assertEqual(ev.main(['report', str(path), str(rows)]), 2)
            self.assertNotIn('private-marker', err.getvalue())

    def test_symlinks_directories_oversize_and_duplicate_keys_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'data'; path.write_text('{}')
            link = Path(root) / 'link'; link.symlink_to(path)
            for candidate in [link, Path(root)]:
                with self.subTest(candidate=candidate), self.assertRaises((ValueError, OSError)):
                    ev.read_json(candidate)
            folder = Path(root) / 'folder'; folder.symlink_to(root, target_is_directory=True)
            with self.assertRaises((ValueError, OSError)):
                ev.read_json(folder / 'data')
            path.write_text('{"a":1,"a":2}')
            with self.assertRaises(ValueError):
                ev.read_json(path)
            with path.open('wb') as f:
                f.truncate(ev.LIMIT + 1)
            with self.assertRaises(ValueError):
                ev.read_json(path)


if __name__ == '__main__':
    unittest.main()
