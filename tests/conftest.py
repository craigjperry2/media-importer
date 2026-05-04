import tempfile

from pathlib import Path
from typing import Any, Generator
import pytest


@pytest.fixture
def workspace() -> Generator[Path, Any, None]:
    with tempfile.TemporaryDirectory() as td:
        yield Path(td)
