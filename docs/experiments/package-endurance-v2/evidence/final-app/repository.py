"""Repository v2: resolution + transactional deployment per SPEC.md."""
import json
import os
from pathlib import Path

from app import deploy, state
from app.resolver import resolve  # noqa: F401  re-exported public API
from app.util import check_name

__all__ = ['resolve', 'Repository']


class Repository:
    """Context manager wrapping a root with sqlite authority + on-disk layout."""

    def __init__(self, root, catalog_path):
        self.root = Path(os.fspath(root))
        self.catalog_path = Path(os.fspath(catalog_path))
        self.catalog = json.loads(self.catalog_path.read_text(encoding='utf-8'))
        self.root.mkdir(parents=True, exist_ok=True)
        for sub in ('objects', 'manifests', 'deployments'):
            (self.root / sub).mkdir(exist_ok=True)
        self._conn = state.connect(self.root)
        version = state.ensure_schema(self._conn)
        if version > 2:
            raise ValueError('unsupported future schema version %r' % (version,))

    # -- lifecycle -----------------------------------------------------------
    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def close(self):
        if self._conn is not None:
            self._conn.close()
            self._conn = None

    @property
    def conn(self):
        return self._conn

    def _resolve(self, requirements):
        return resolve(self.catalog, requirements)

    # -- public API ----------------------------------------------------------
    def install(self, tenant, environment, key, requirements, expected_generation=None,
                crash_at=None):
        check_name(tenant, 'tenant')
        check_name(environment, 'environment')
        check_name(key, 'key')
        if crash_at not in (None, 'before_pointer', 'after_pointer'):
            raise ValueError('unknown crash_at %r' % (crash_at,))
        return deploy.install(self, tenant, environment, key, requirements,
                              expected_generation, crash_at)

    def recover(self):
        return deploy.reconcile(self)

    def active(self, tenant, environment):
        check_name(tenant, 'tenant')
        check_name(environment, 'environment')
        pointer = deploy.read_pointer(self.root, tenant, environment)
        if pointer is None or not deploy.pointer_valid(pointer):
            return None
        row = state.read_receipt(self._conn, tenant, environment, pointer['key'])
        if row is None:
            return None
        if row['manifest_sha256'] != pointer['manifest_sha256']:
            return None
        if row['generation'] != pointer['generation']:
            return None
        if deploy.read_verified(
                deploy.manifest_path(self.root, pointer['manifest_sha256']),
                pointer['manifest_sha256']) is None:
            return None
        return pointer

    def receipt(self, tenant, environment, key):
        check_name(tenant, 'tenant')
        check_name(environment, 'environment')
        check_name(key, 'key')
        row = state.read_receipt(self._conn, tenant, environment, key)
        if row is None:
            return None
        return deploy.receipt_obj(tenant, environment, key, row['generation'],
                                  row['manifest_sha256'])

    def gc(self):
        return deploy.gc(self)

    def migrate(self, crash_at=None):
        if crash_at not in (None, 'migration_after_copy'):
            raise ValueError('unknown crash_at %r' % (crash_at,))
        return deploy.migrate(self, crash_at)

    def publish(self, tenant, environment, key, url):
        check_name(tenant, 'tenant')
        check_name(environment, 'environment')
        check_name(key, 'key')
        return deploy.publish(self, tenant, environment, key, url)
