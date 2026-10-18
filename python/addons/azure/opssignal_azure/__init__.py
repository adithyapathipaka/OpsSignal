"""Azure Blob Storage and App Configuration support for opssignal.

Installed by `pip install 'opssignal[azure]'`. Nothing to import or
configure: opssignal finds this package on its own and uses it when
SIGNAL_CONFIG is an az:// or azappconfig:// URL.
"""

from opssignal_azure._azure import CLOUD, fetch

__all__ = ["CLOUD", "fetch"]
