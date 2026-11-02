"""Stand-in for Decky Loader's `decky` module, used by the tests."""
import logging
import os
import tempfile

DECKY_PLUGIN_SETTINGS_DIR = os.environ.get("DECKY_TEST_SETTINGS") or tempfile.mkdtemp(prefix="decky-settings-")
DECKY_USER_HOME = os.path.expanduser("~")
logger = logging.getLogger("decky")


async def emit(*_args):
    pass
