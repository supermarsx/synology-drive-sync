include /env.mak

JS_DIR="dist"
JS_NAMESPACE="SYNO.SDS.App.SynologyDriveSync"
BUNDLE_JS="dist/SynologyDriveSync.js"
BUNDLE_CSS="dist/style.css"
UI_ASSET_DIR?=../package/ui

.PHONY: all native-build clean install packageinstall

all: native-build style.css

native-build:
	/usr/local/tool/snpm install
	/usr/local/tool/snpm run build
	$(MAKE) -f Makefile.js.inc JSCompress JS_NAMESPACE="${JS_NAMESPACE}" JS_DIR=${JS_DIR}

style.css: native-build
	cp $(BUNDLE_CSS) $@

clean: clean_JSCompress
	rm -f $(BUNDLE_JS) $(BUNDLE_CSS) style.css

install: install_JSCompress
	[ -d $(INSTALLDIR)/texts ] || install -d $(INSTALLDIR)/texts
	[ -d $(INSTALLDIR)/images ] || install -d $(INSTALLDIR)/images
	cp -a $(UI_ASSET_DIR)/texts $(INSTALLDIR)
	cp -a $(UI_ASSET_DIR)/images $(INSTALLDIR)
	cp style.css $(INSTALLDIR)

packageinstall:

include Makefile.js.inc
