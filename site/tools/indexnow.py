"""Tell Bing, Yandex, Seznam and other IndexNow search engines that the site's pages changed.

Run from the repository root, only after the site is live on https://prismdl.site/:
    python site/tools/indexnow.py            # every page in site/sitemap.xml
    python site/tools/indexnow.py /windows/  # just these pages

The key is the name of the site/<key>.txt file, which must be published too (search engines
fetch it to check the request comes from the site's owner). Google doesn't use IndexNow:
for Google, use Search Console (see SEO.md). Standard library only.
"""
import glob
import json
import os
import re
import sys
import urllib.request

SITE = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
HOST = "prismdl.site"

keys = [os.path.basename(p)[:-4] for p in glob.glob(os.path.join(SITE, "*.txt"))
        if re.fullmatch(r"[0-9a-f]{32}", os.path.basename(p)[:-4])]
if len(keys) != 1:
    sys.exit("indexnow: expected exactly one site/<32 hex>.txt key file")
key = keys[0]

if len(sys.argv) > 1:
    urls = [f"https://{HOST}{p}" for p in sys.argv[1:]]
else:
    with open(os.path.join(SITE, "sitemap.xml"), encoding="utf-8") as f:
        urls = re.findall(r"<loc>(.*?)</loc>", f.read())

body = json.dumps({"host": HOST, "key": key, "keyLocation": f"https://{HOST}/{key}.txt", "urlList": urls}).encode()
req = urllib.request.Request("https://api.indexnow.org/indexnow", data=body,
                             headers={"Content-Type": "application/json; charset=utf-8"})
with urllib.request.urlopen(req, timeout=30) as r:
    # 200 = accepted, 202 = accepted, key check pending
    print(r.status, "for", len(urls), "URL(s)")
