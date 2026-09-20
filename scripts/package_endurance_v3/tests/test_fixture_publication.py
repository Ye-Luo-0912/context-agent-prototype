"""Independent loopback receiver evidence for persisted publication recovery."""
import json
from contextlib import closing
import sqlite3
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

from test_fixture_transactions import FixtureHarness, raw


class FixturePublication(FixtureHarness):
    def setUp(self):
        super().setUp()
        self.receipt = self.success('install', **self.request())
        self.audit = []
        self.committed = None
        self.entered = threading.Event()
        self.release = threading.Event()
        self.block_post = False
        self.drop_post_response = False
        self.get_status = None
        self.post_status = 200
        self.wrong_ack = False
        test = self

        class Receiver(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def respond(self, status, payload):
                body = raw(payload)
                try:
                    self.send_response(status)
                    self.send_header('Content-Length', str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except OSError:
                    pass

            def do_GET(self):
                query = parse_qs(urlsplit(self.path).query)
                test.audit.append(dict(method='GET', query=query))
                expected = {key: [str(test.receipt[key])] for key in ('tenant', 'environment', 'key')}
                if test.get_status is not None:
                    self.respond(test.get_status, {})
                elif query != expected or test.committed is None:
                    self.respond(404, {})
                else:
                    payload = dict(test.committed, generation=999) if test.wrong_ack else test.committed
                    self.respond(200, payload)

            def do_POST(self):
                body = self.rfile.read(int(self.headers['Content-Length']))
                test.audit.append(dict(method='POST', body=body.decode()))
                if test.post_status == 200:
                    test.committed = json.loads(body)
                test.entered.set()
                if test.block_post:
                    test.release.wait(15)
                if test.drop_post_response:
                    self.close_connection = True
                else:
                    payload = dict(test.committed, key='wrong') if test.wrong_ack and test.committed else test.committed
                    self.respond(test.post_status, payload)

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Receiver)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop_server)
        self.kwargs = dict(tenant='tenant-a', environment='prod', key='first',
                           url='http://127.0.0.1:%d/' % self.server.server_port)

    def stop_server(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)

    def outbox(self):
        with closing(sqlite3.connect(self.root / 'repo.sqlite')) as db:
            row = db.execute('SELECT body,status,ack,url FROM outbox').fetchone()
        return row

    def test_receiver_commits_process_killed_reopen_queries_without_post(self):
        self.block_post = True
        child = self.spawn('publish', **self.kwargs)
        self.assertTrue(self.entered.wait(10), self.audit)
        # Read pending durable evidence while sender is in the network call.
        pending = self.outbox()
        self.assertEqual(pending[:3], (raw(self.receipt).decode(), 'pending', None))
        self.assertEqual(pending[3], self.kwargs['url'])
        child.kill()
        child.communicate(timeout=10)
        self.assertNotEqual(child.returncode, 0)
        self.release.set()
        response = self.success('publish', **self.kwargs)
        self.assertEqual(response['acknowledgement'], self.receipt)
        self.assertEqual([row['method'] for row in self.audit], ['GET', 'POST', 'GET'])
        self.assertEqual(json.loads(self.audit[1]['body']), self.receipt)
        self.check_disk(self.receipt)
        self.assertEqual(self.outbox()[1], 'acknowledged')

    def test_concurrent_publishers_commit_one_post_and_reuse_ack(self):
        children = [self.spawn('publish', **self.kwargs) for _ in range(4)]
        responses = [self.finish(child) for child in children]
        self.assertTrue(all(row['ok'] for row in responses), responses)
        self.assertEqual([row['method'] for row in self.audit], ['GET', 'POST'])
        for row in responses:
            self.assertEqual(row['value']['acknowledgement'], self.receipt)

    def test_transport_lost_after_commit_queries_original_identity(self):
        self.drop_post_response = True
        result = self.success('publish', **self.kwargs)
        self.assertEqual(result['acknowledgement'], self.receipt)
        self.assertEqual([row['method'] for row in self.audit], ['GET', 'POST', 'GET'])

    def test_query_503_retains_pending_and_never_posts_until_definite_404(self):
        self.get_status = 503
        result = self.call('publish', **self.kwargs)
        self.assertFalse(result['ok'], result)
        self.assertEqual([row['method'] for row in self.audit], ['GET'])
        self.assertEqual(self.outbox()[1], 'pending')
        self.get_status = None
        self.success('publish', **self.kwargs)
        self.assertEqual([row['method'] for row in self.audit], ['GET', 'GET', 'POST'])

    def test_wrong_post_and_get_ack_never_become_success(self):
        self.wrong_ack = True
        first = self.call('publish', **self.kwargs)
        self.assertFalse(first['ok'], first)
        self.assertEqual(self.outbox()[1], 'pending')
        second = self.call('publish', **self.kwargs)
        self.assertFalse(second['ok'], second)
        self.assertEqual(self.outbox()[1], 'pending')
        self.assertEqual([row['method'] for row in self.audit], ['GET', 'POST', 'GET'])
        self.wrong_ack = False
        self.success('publish', **self.kwargs)
        self.assertEqual(sum(row['method'] == 'POST' for row in self.audit), 1)

    def test_original_url_is_bound_before_transport(self):
        self.get_status = 503
        self.assertFalse(self.call('publish', **self.kwargs)['ok'])
        wrong = dict(self.kwargs, url=self.kwargs['url'] + 'different/')
        result = self.call('publish', **wrong)
        self.assertFalse(result['ok'], result)
        self.assertEqual(len(self.audit), 1)
        self.assertEqual(self.outbox()[3], self.kwargs['url'])

    def test_conflict_is_durable_and_never_retried(self):
        self.post_status = 409
        result = self.call('publish', **self.kwargs)
        self.assertFalse(result['ok'], result)
        self.assertEqual(self.outbox()[1], 'conflict')
        self.assertFalse(self.call('publish', **self.kwargs)['ok'])
        self.assertEqual([row['method'] for row in self.audit], ['GET', 'POST'])


if __name__ == '__main__':
    unittest.main()
