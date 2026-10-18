"""Azure Blob Storage support for opssignal.

Installed by `pip install 'opssignal[azure]'`. Nothing to import or
configure: opssignal finds this package on its own and uses it when
SIGNAL_CONFIG is a az:// URL.
"""

from opssignal_azure._azure import SCHEME, fetch

__all__ = ["SCHEME", "fetch"]
