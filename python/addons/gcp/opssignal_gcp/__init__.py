"""Google Cloud Storage support for opssignal.

Installed by `pip install 'opssignal[gcp]'`. Nothing to import or
configure: opssignal finds this package on its own and uses it when
SIGNAL_CONFIG is a gs:// URL.
"""

from opssignal_gcp._gcp import SCHEME, fetch

__all__ = ["SCHEME", "fetch"]
