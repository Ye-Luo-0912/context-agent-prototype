"""Independent checks added by the platform upgrade (P3 concurrent boundary).

These tests are *additive*: they live under ``app/tests`` so the immutable
``tests/`` directory and ``fixtures/`` are never touched. They spawn real OS
worker processes against the application queue/CAS and assert the concurrency
contracts directly.
"""
