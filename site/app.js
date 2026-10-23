(function () {
  var REL = "https://github.com/transit-audio/transit/releases/latest";
  var meta = document.getElementById("release-meta");
  var list = document.getElementById("assets");
  var PLATFORMS = [
    ["Windows installer (MSI)", /\.msi$/i],
    ["Windows (zip)", /\.zip$/i],
    ["macOS (DMG)", /\.dmg$/i],
    ["Linux (tar.gz)", /\.tar\.gz$/i]
  ];
  function row(label, href, note) {
    var li = document.createElement("li");
    var a = document.createElement("a");
    a.href = href;
    a.textContent = label;
    li.appendChild(a);
    if (note) {
      var s = document.createElement("span");
      s.className = "muted";
      s.textContent = note;
      li.appendChild(s);
    }
    list.appendChild(li);
  }
  function fallback() {
    meta.textContent = "Grab the newest build from GitHub.";
    list.textContent = "";
    PLATFORMS.forEach(function (p) { row(p[0], REL); });
  }
  fetch("https://api.github.com/repos/transit-audio/transit/releases/latest", { headers: { Accept: "application/vnd.github+json" } })
    .then(function (r) { if (!r.ok) throw new Error(r.status); return r.json(); })
    .then(function (rel) {
      var date = rel.published_at ? new Date(rel.published_at).toLocaleDateString(undefined, { year: "numeric", month: "long", day: "numeric" }) : "";
      meta.textContent = (rel.name || rel.tag_name) + (date ? ", released " + date : "");
      var assets = rel.assets || [], found = 0;
      PLATFORMS.forEach(function (p) {
        var a = assets.filter(function (x) { return p[1].test(x.name); })[0];
        if (a) {
          found++;
          row(p[0], a.browser_download_url, a.name + " (" + (a.size / 1048576).toFixed(1) + " MB)");
        } else {
          row(p[0], REL, "see the release page");
        }
      });
      if (!found) fallback();
    })
    .catch(fallback);
})();
