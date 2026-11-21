#!/bin/sh
# Launch Playwright's WebKit on a host its Ubuntu build does not support.
#
# Playwright's WebKit is built on Ubuntu 24.04 and links three libraries an
# Arch install does not have at those sonames: ICU 74, libxml2.so.2 and
# flite. `fetch-compat-libs.sh` puts Ubuntu's own copies in a cache
# directory; this wrapper is what makes WebKit find them.
#
# It replaces the bundle's own `MiniBrowser` wrapper rather than calling it,
# because that wrapper sets LD_LIBRARY_PATH outright and would drop the
# compat directory. What it exports is otherwise the same as the bundle's
# wrapper plus `pw_run.sh` (WEBKIT_FORCE_COMPLEX_TEXT), so a Playwright
# upgrade that changes either is the thing to check if WebKit stops starting.
#
# The config passes:
#   DJ_WEBKIT_DIR          Playwright's webkit-<rev> directory
#   DJ_WEBKIT_COMPAT_LIBS  the directory holding the compat libraries
set -eu

case " $* " in
  *" --headless "*) flavor=wpe ;;
  *) flavor=gtk ;;
esac

dir="${DJ_WEBKIT_DIR:?DJ_WEBKIT_DIR is not set}/minibrowser-$flavor"
compat="${DJ_WEBKIT_COMPAT_LIBS:?DJ_WEBKIT_COMPAT_LIBS is not set}"

export WEBKIT_EXEC_PATH="$dir/bin"
export WEBKIT_INJECTED_BUNDLE_PATH="$dir/lib"
if [ -d "$dir/share" ]; then
  export WEBKIT_INSPECTOR_RESOURCES_PATH="$dir/share"
fi
export LD_LIBRARY_PATH="$dir/lib:$dir/sys/lib:$compat"
export WEBKIT_FORCE_COMPLEX_TEXT=1

exec "$dir/bin/MiniBrowser" "$@"
