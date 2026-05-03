import tempfile

import pytest


@pytest.fixture
def workspace():
    with tempfile.TemporaryDirectory() as td:
        yield td
