package main

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

// fakeCurl serves release metadata and assets from its own directory: the
// metadata URL returns release.json and an asset URL returns asset.<id>, so
// pairing an asset with its uploader's id fails the download.
const fakeCurl = `#!/bin/sh
d="$(dirname "$0")"
out=
while [ "$#" -gt 1 ]; do
  [ "$1" = --output ] && out=$2
  shift
done
printf '%s\n' "$1" >> "$d/curl.log"
case $1 in
*/releases/latest) cp "$d/release.json" "$out" ;;
*/releases/assets/*) cp "$d/asset.${1##*/}" "$out" 2>/dev/null || exit 22 ;;
*/releases/latest/download/*|*/releases/download/*/*) cp "$d/public.${1##*/}" "$out" 2>/dev/null || exit 22 ;;
*) exit 22 ;;
esac
`

// releaseJSON builds compact release metadata in GitHub's shape: top-level and
// author ids, braces in upload_url and body, and in every asset an uploader
// object with its own id and {/…} URL templates. uploaderFirst moves uploader
// ahead of the asset's url, id and name.
func releaseJSON(names []string, uploaderFirst bool, login string) string {
	var assets []string
	for i, name := range names {
		id := 101 + i
		u := "https://api.github.com/users/olafurns7"
		uploader := fmt.Sprintf(`"uploader":{"login":"%s","id":%d,"node_id":"U_1",`, login, 900+i) +
			`"avatar_url":"https://avatars.githubusercontent.com/u/7?v=4","gravatar_id":"","url":"` + u + `",` +
			`"html_url":"https://github.com/olafurns7","followers_url":"` + u + `/followers",` +
			`"following_url":"` + u + `/following{/other_user}","gists_url":"` + u + `/gists{/gist_id}",` +
			`"starred_url":"` + u + `/starred{/owner}{/repo}","subscriptions_url":"` + u + `/subscriptions",` +
			`"organizations_url":"` + u + `/orgs","repos_url":"` + u + `/repos","events_url":"` + u + `/events{/privacy}",` +
			`"received_events_url":"` + u + `/received_events","type":"User","user_view_type":"public","site_admin":false}`
		fields := []string{
			fmt.Sprintf(`"url":"https://api.github.com/repos/olafurns7/herdr-taskr/releases/assets/%d"`, id),
			fmt.Sprintf(`"id":%d`, id), `"node_id":"RA_1"`, fmt.Sprintf(`"name":"%s"`, name), `"label":""`, uploader,
			`"content_type":"application/octet-stream"`, `"state":"uploaded"`, `"size":10`, `"download_count":0`,
			fmt.Sprintf(`"browser_download_url":"https://github.com/olafurns7/herdr-taskr/releases/download/v9.9.9/%s"`, name),
		}
		if uploaderFirst {
			fields = append([]string{uploader}, append(fields[:5:5], fields[6:]...)...)
		}
		assets = append(assets, "{"+strings.Join(fields, ",")+"}")
	}
	return `{"url":"https://api.github.com/repos/olafurns7/herdr-taskr/releases/1","upload_url":"https://uploads.github.com/repos/olafurns7/herdr-taskr/releases/1/assets{?name,label}",` +
		`"id":1,"author":{"login":"olafurns7","id":7},"tag_name":"v9.9.9","assets":[` + strings.Join(assets, ",") +
		`],"tarball_url":"https://api.github.com/repos/olafurns7/herdr-taskr/tarball/v9.9.9","body":"notes: {\"id\":5} [x]"}`
}

func TestInstallTokenTransport(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("install.sh supports darwin and linux only")
	}
	host := "taskr-" + runtime.GOOS + "-" + runtime.GOARCH
	names := []string{"taskr-darwin-amd64", "taskr-darwin-arm64", "taskr-linux-amd64", "taskr-linux-arm64", "SHA256SUMS", "skill.tar.gz", "plugin.tar.gz"}
	plugin := pluginTarball(t)
	skill := skillTarball(t)
	compact := releaseJSON(names, false, "olafurns7")
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, []byte(releaseJSON(names, true, "olafurns7")), "", "  "); err != nil {
		t.Fatal(err)
	}
	// Release assets uploaded by Actions carry "login":"github-actions[bot]";
	// its "]" must not end the assets array before the later assets are read.
	botCompact := releaseJSON(names, false, "github-actions[bot]")
	var botPretty bytes.Buffer
	if err := json.Indent(&botPretty, []byte(releaseJSON(names, true, "github-actions[bot]")), "", "  "); err != nil {
		t.Fatal(err)
	}
	for _, tc := range []struct {
		name, metadata string
		lines          int
	}{
		{"compact", compact, 1},
		{"pretty", pretty.String(), -1},
		{"compact-bot-uploader", botCompact, 1},
		{"pretty-bot-uploader", botPretty.String(), -1},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if n := strings.Count(tc.metadata, "\n") + 1; tc.lines > 0 && n != tc.lines || tc.lines < 0 && n < 20 {
				t.Fatalf("fixture has %d lines", n)
			}
			dir := t.TempDir()
			bin := filepath.Join(dir, "bin")
			os.MkdirAll(bin, 0o755)
			write := func(name, content string, mode os.FileMode) {
				t.Helper()
				if err := os.WriteFile(filepath.Join(bin, name), []byte(content), mode); err != nil {
					t.Fatal(err)
				}
			}
			write("curl", fakeCurl, 0o755)
			write("gh", "#!/bin/sh\nexit 1\n", 0o755)
			write("release.json", tc.metadata, 0o644)
			var sums strings.Builder
			for i, name := range names[:4] {
				body := fmt.Sprintf("#!/bin/sh\necho v9.9.9-%s\n", name)
				write(fmt.Sprintf("asset.%d", 101+i), body, 0o644)
				fmt.Fprintf(&sums, "%x  %s\n", sha256.Sum256([]byte(body)), name)
			}
			fmt.Fprintf(&sums, "%x  plugin.tar.gz\n%x  skill.tar.gz\n", sha256.Sum256(plugin), sha256.Sum256(skill))
			write("asset.105", sums.String(), 0o644)
			write("asset.106", string(skill), 0o644)
			write("asset.107", string(plugin), 0o644)

			installDir, skillDir := filepath.Join(dir, "inst"), filepath.Join(dir, "skill")
			cmd := exec.Command("sh", "install.sh")
			cmd.Env = []string{"PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"), "HOME=" + dir,
				"GH_TOKEN=test-token", "TASKR_INSTALL_DIR=" + installDir, "TASKR_SKILL_DIR=" + skillDir}
			out, err := cmd.CombinedOutput()
			t.Logf("install.sh (%s metadata):\n%s", tc.name, out)
			if err != nil {
				t.Fatalf("install.sh: %v", err)
			}
			got, _ := exec.Command(filepath.Join(installDir, "taskr")).Output()
			if want := "v9.9.9-" + host + "\n"; string(got) != want {
				t.Fatalf("installed taskr prints %q, want %q", got, want)
			}
			if b, _ := os.ReadFile(filepath.Join(skillDir, "SKILL.md")); string(b) != "# taskr skill\n" {
				t.Fatalf("installed skill = %q", b)
			}
			pluginDir := filepath.Join(dir, ".local", "share", "taskr", "plugin")
			if b, _ := os.ReadFile(filepath.Join(pluginDir, "run.sh")); !strings.Contains(string(b), "taskr daemon") {
				t.Fatalf("installed run.sh = %q", b)
			}
			if !strings.Contains(string(out), `MANUAL: inside Herdr, run: herdr plugin link "`+pluginDir+`"`) {
				t.Fatalf("install.sh outside Herdr did not print the MANUAL link line")
			}
			log, _ := os.ReadFile(filepath.Join(bin, "curl.log"))
			for _, id := range []int{101, 102, 103, 104} {
				if names[id-101] == host && !strings.Contains(string(log), fmt.Sprintf("/releases/assets/%d\n", id)) {
					t.Fatalf("curl calls = %q, want asset %d for %s", log, id, host)
				}
			}
			if strings.Contains(string(log), "/releases/assets/9") || strings.Contains(string(log), "test-token") {
				t.Fatalf("curl calls = %q: uploader id or token in a URL", log)
			}
		})
	}

	t.Run("missing", func(t *testing.T) {
		dir := t.TempDir()
		bin := filepath.Join(dir, "bin")
		os.MkdirAll(bin, 0o755)
		os.WriteFile(filepath.Join(bin, "curl"), []byte(fakeCurl), 0o755)
		os.WriteFile(filepath.Join(bin, "gh"), []byte("#!/bin/sh\nexit 1\n"), 0o755)
		os.WriteFile(filepath.Join(bin, "release.json"), []byte(releaseJSON([]string{"SHA256SUMS", "SKILL.md"}, false, "olafurns7")), 0o644)
		cmd := exec.Command("sh", "install.sh")
		cmd.Env = []string{"PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"), "HOME=" + dir,
			"GH_TOKEN=test-token", "TASKR_INSTALL_DIR=" + filepath.Join(dir, "inst")}
		out, err := cmd.CombinedOutput()
		if err == nil || !strings.Contains(string(out), "release asset not found: "+host) {
			t.Fatalf("install.sh without %s: err %v, output %q", host, err, out)
		}
	})
}

func TestInstallAnonymousTransport(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("install.sh supports darwin and linux only")
	}
	host := "taskr-" + runtime.GOOS + "-" + runtime.GOARCH
	plugin, skill := pluginTarball(t), skillTarball(t)
	for _, tc := range []struct {
		name, tag   string
		badChecksum bool
	}{
		{"latest", "", false},
		{"tagged", "v9.9.9", false},
		{"bad-checksum", "", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			dir := t.TempDir()
			bin := filepath.Join(dir, "bin")
			if err := os.MkdirAll(bin, 0o755); err != nil {
				t.Fatal(err)
			}
			write := func(name, content string, mode os.FileMode) {
				t.Helper()
				if err := os.WriteFile(filepath.Join(bin, name), []byte(content), mode); err != nil {
					t.Fatal(err)
				}
			}
			write("curl", fakeCurl, 0o755)
			write("gh", "#!/bin/sh\nexit 1\n", 0o755)
			body := "#!/bin/sh\necho anonymous-" + host + "\n"
			assets := map[string][]byte{
				host: []byte(body), "plugin.tar.gz": plugin, "skill.tar.gz": skill,
			}
			var sums strings.Builder
			for asset, content := range assets {
				download := content
				if tc.badChecksum && asset == host {
					download = []byte("corrupt\n")
				}
				write("public."+asset, string(download), 0o644)
				fmt.Fprintf(&sums, "%x  %s\n", sha256.Sum256(content), asset)
			}
			assets["SHA256SUMS"] = []byte(sums.String())
			write("public.SHA256SUMS", sums.String(), 0o644)

			installDir, skillDir := filepath.Join(dir, "inst"), filepath.Join(dir, "skill")
			env := []string{"PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"), "HOME=" + dir,
				"TASKR_INSTALL_DIR=" + installDir, "TASKR_SKILL_DIR=" + skillDir}
			if tc.tag != "" {
				env = append(env, "TASKR_VERSION="+tc.tag)
			}
			cmd := exec.Command("sh", "install.sh")
			cmd.Env = env
			out, err := cmd.CombinedOutput()
			if tc.badChecksum {
				if err == nil || !strings.Contains(string(out), "checksum verification failed: "+host) {
					t.Errorf("install.sh with bad checksum: %v\n%s", err, out)
				}
				for _, path := range []string{
					filepath.Join(installDir, "taskr"),
					filepath.Join(skillDir, "SKILL.md"),
					filepath.Join(dir, ".local", "share", "taskr", "plugin", "run.sh"),
				} {
					if _, err := os.Stat(path); !os.IsNotExist(err) {
						t.Fatalf("bad checksum installed %s: %v", path, err)
					}
				}
				return
			}
			if err != nil {
				t.Fatalf("install.sh: %v\n%s", err, out)
			}
			got, err := exec.Command(filepath.Join(installDir, "taskr")).Output()
			if err != nil || string(got) != "anonymous-"+host+"\n" {
				t.Fatalf("installed taskr = %q, %v", got, err)
			}
			if b, err := os.ReadFile(filepath.Join(skillDir, "SKILL.md")); err != nil || string(b) != "# taskr skill\n" {
				t.Fatalf("installed skill = %q, %v", b, err)
			}
			if _, err := os.Stat(filepath.Join(dir, ".local", "share", "taskr", "plugin", "run.sh")); err != nil {
				t.Fatalf("installed plugin: %v", err)
			}
			log, err := os.ReadFile(filepath.Join(bin, "curl.log"))
			if err != nil {
				t.Fatal(err)
			}
			for asset := range assets {
				path := "/releases/latest/download/" + asset
				if tc.tag != "" {
					path = "/releases/download/" + tc.tag + "/" + asset
				}
				if !strings.Contains(string(log), path+"\n") {
					t.Fatalf("curl calls = %q, missing %s", log, path)
				}
			}
			if strings.Contains(string(log), "api.github.com") || strings.Contains(string(log), "test-token") {
				t.Fatalf("anonymous curl calls include authenticated transport: %q", log)
			}
		})
	}
}

// pluginTarball packs the repository's plugin/ directory the way release.sh does.
func pluginTarball(t *testing.T) []byte {
	t.Helper()
	var buf bytes.Buffer
	gz := gzip.NewWriter(&buf)
	tw := tar.NewWriter(gz)
	for _, name := range []string{"run.sh", "herdr-plugin.toml"} {
		b, err := os.ReadFile(filepath.Join("plugin", name))
		if err != nil {
			t.Fatal(err)
		}
		tw.WriteHeader(&tar.Header{Name: name, Mode: 0o755, Size: int64(len(b)), Typeflag: tar.TypeReg})
		tw.Write(b)
	}
	tw.Close()
	gz.Close()
	return buf.Bytes()
}

// TestInstallPluginLink covers the plugin step inside Herdr: link when the id
// is absent, skip when present, and nothing at all with TASKR_NO_PLUGIN=1.
func TestInstallPluginLink(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("install.sh supports darwin and linux only")
	}
	host := "taskr-" + runtime.GOOS + "-" + runtime.GOARCH
	plugin := pluginTarball(t)
	skill := skillTarball(t)
	for _, tc := range []struct {
		name, list, noPlugin, corrupt string
		link, fail                    bool
	}{
		{name: "absent", list: `{"plugins":[{"id":"hhdebb.herdr-radar"}]}`, link: true},
		{name: "present", list: `{"plugins":[{"id":"olafurns7.taskr","enabled":true}]}`},
		{name: "no-plugin", list: `[]`, noPlugin: "1"},
		{name: "corrupt", list: `[]`, corrupt: "x", fail: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			dir := t.TempDir()
			bin := filepath.Join(dir, "bin")
			os.MkdirAll(bin, 0o755)
			write := func(name, content string, mode os.FileMode) {
				t.Helper()
				if err := os.WriteFile(filepath.Join(bin, name), []byte(content), mode); err != nil {
					t.Fatal(err)
				}
			}
			write("curl", fakeCurl, 0o755)
			write("gh", "#!/bin/sh\nexit 1\n", 0o755)
			write("herdr", "#!/bin/sh\nd=\"$(dirname \"$0\")\"\nprintf '%s|' \"$@\" >> \"$d/herdr.log\"\necho >> \"$d/herdr.log\"\n"+
				"[ \"$1 $2\" = \"plugin list\" ] && cat \"$d/plugins.json\"\nexit 0\n", 0o755)
			write("plugins.json", tc.list, 0o644)
			names := []string{host, "SHA256SUMS", "skill.tar.gz", "plugin.tar.gz"}
			write("release.json", releaseJSON(names, false, "olafurns7"), 0o644)
			body := "#!/bin/sh\necho ok\n"
			write("asset.101", body, 0o644)
			write("asset.102", fmt.Sprintf("%x  %s\n%x  plugin.tar.gz\n%x  skill.tar.gz\n", sha256.Sum256([]byte(body)), host, sha256.Sum256(plugin), sha256.Sum256(skill)), 0o644)
			write("asset.103", string(skill), 0o644)
			write("asset.104", string(plugin)+tc.corrupt, 0o644)
			cmd := exec.Command("sh", "install.sh")
			cmd.Env = []string{"PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"), "HOME=" + dir,
				"GH_TOKEN=test-token", "HERDR_ENV=1", "TASKR_NO_PLUGIN=" + tc.noPlugin}
			out, err := cmd.CombinedOutput()
			t.Logf("install.sh:\n%s", out)
			if tc.fail {
				if err == nil || !strings.Contains(string(out), "checksum verification failed: plugin.tar.gz") {
					t.Fatalf("corrupt plugin: err %v", err)
				}
				return
			}
			if err != nil {
				t.Fatalf("install.sh: %v", err)
			}
			pluginDir := filepath.Join(dir, ".local", "share", "taskr", "plugin")
			calls, _ := os.ReadFile(filepath.Join(bin, "herdr.log"))
			curls, _ := os.ReadFile(filepath.Join(bin, "curl.log"))
			switch {
			case tc.noPlugin == "1":
				if _, err := os.Stat(pluginDir); err == nil || len(calls) != 0 || strings.Contains(string(curls), "/assets/104") {
					t.Fatalf("TASKR_NO_PLUGIN=1: plugin dir exists=%v herdr calls %q curl %q", err == nil, calls, curls)
				}
			case tc.link:
				if want := "plugin|list|--json|\nplugin|link|" + pluginDir + "|\n"; string(calls) != want {
					t.Fatalf("herdr calls = %q, want %q", calls, want)
				}
			default:
				if want := "plugin|list|--json|\n"; string(calls) != want || !strings.Contains(string(out), "already linked") {
					t.Fatalf("herdr calls = %q, want %q", calls, want)
				}
			}
			if tc.noPlugin == "" {
				if b, _ := os.ReadFile(filepath.Join(pluginDir, "herdr-plugin.toml")); !strings.Contains(string(b), `id = "olafurns7.taskr"`) {
					t.Fatalf("installed manifest = %q", b)
				}
			}
		})
	}
}

// TestInstallSkillPermsNoAgentLinks: the skill is installed 755/644 whatever
// the script's umask (an older install's 700/600 is repaired), a
// TASKR_SKILL_DIR override keeps working, and no agent-specific path
// (~/.claude, ~/.codex, ~/.config/opencode) is created or touched by default.
func TestInstallSkillPermsNoAgentLinks(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("install.sh supports darwin and linux only")
	}
	host := "taskr-" + runtime.GOOS + "-" + runtime.GOARCH
	skill := skillTarball(t)
	hint := "skill: optional links: set TASKR_LINK_SKILLS=1 to link into existing agent configs"
	agentPaths := []string{".claude", ".codex", filepath.Join(".config", "opencode")}
	for _, tc := range []struct {
		name     string
		agents   bool // HOME already has agent dirs with their own skills
		old      bool // an earlier install left 700/600
		override bool
	}{
		{name: "fresh-home"},
		{name: "agents-present-old-install", agents: true, old: true},
		{name: "override", agents: true, override: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			dir := t.TempDir()
			bin := filepath.Join(dir, "bin")
			os.MkdirAll(bin, 0o755)
			write := func(name, content string) {
				if err := os.WriteFile(filepath.Join(bin, name), []byte(content), 0o755); err != nil {
					t.Fatal(err)
				}
			}
			write("curl", fakeCurl)
			write("gh", "#!/bin/sh\nexit 1\n")
			write("release.json", releaseJSON([]string{host, "SHA256SUMS", "skill.tar.gz"}, false, "olafurns7"))
			body := "#!/bin/sh\necho ok\n"
			write("asset.101", body)
			write("asset.102", fmt.Sprintf("%x  %s\n%x  skill.tar.gz\n", sha256.Sum256([]byte(body)), host, sha256.Sum256(skill)))
			write("asset.103", string(skill))

			if tc.agents {
				os.MkdirAll(filepath.Join(dir, ".claude", "skills"), 0o755)
				os.Symlink("../../.agents/skills/herdr", filepath.Join(dir, ".claude", "skills", "herdr"))
				os.MkdirAll(filepath.Join(dir, ".codex", "skills"), 0o755)
				os.MkdirAll(filepath.Join(dir, ".config", "opencode"), 0o755)
			}
			skillDir := filepath.Join(dir, ".agents", "skills", "taskr")
			if tc.old {
				os.MkdirAll(skillDir, 0o700)
				os.Chmod(skillDir, 0o700)
				os.WriteFile(filepath.Join(skillDir, "SKILL.md"), []byte("old"), 0o600)
			}
			snapshot := func() string {
				var b strings.Builder
				for _, p := range agentPaths {
					filepath.Walk(filepath.Join(dir, p), func(path string, info os.FileInfo, err error) error {
						if err == nil {
							fmt.Fprintf(&b, "%s %v\n", path, info.Mode())
						}
						return nil
					})
				}
				return b.String()
			}
			before := snapshot()

			env := []string{"PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"), "HOME=" + dir,
				"GH_TOKEN=test-token", "TASKR_NO_PLUGIN=1", "TASKR_INSTALL_DIR=" + filepath.Join(dir, "inst")}
			if tc.override {
				skillDir = filepath.Join(dir, "custom path", "skill")
				env = append(env, "TASKR_SKILL_DIR="+skillDir)
			}
			cmd := exec.Command("sh", "install.sh")
			cmd.Env = env
			out, err := cmd.CombinedOutput()
			t.Logf("install.sh:\n%s", out)
			if err != nil {
				t.Fatalf("install.sh: %v", err)
			}
			for path, want := range map[string]os.FileMode{skillDir: 0o755 | os.ModeDir, filepath.Join(skillDir, "SKILL.md"): 0o644, filepath.Join(skillDir, "references"): 0o755 | os.ModeDir, filepath.Join(skillDir, "references", "format.md"): 0o644, filepath.Join(skillDir, "references", "orchestrator.md"): 0o644} {
				st, err := os.Stat(path)
				if err != nil || st.Mode() != want {
					t.Fatalf("%s stat = %v (%v), want mode %v", path, st, err, want)
				}
			}
			if b, _ := os.ReadFile(filepath.Join(skillDir, "SKILL.md")); string(b) != "# taskr skill\n" {
				t.Fatalf("skill = %q", b)
			}
			if after := snapshot(); after != before {
				t.Fatalf("agent paths changed:\nbefore\n%s\nafter\n%s", before, after)
			}
			if !tc.agents {
				for _, p := range agentPaths {
					if _, err := os.Lstat(filepath.Join(dir, p)); err == nil {
						t.Fatalf("install.sh created %s", p)
					}
				}
			}
			if tc.override {
				if _, err := os.Stat(filepath.Join(dir, ".agents")); err == nil {
					t.Fatal("with TASKR_SKILL_DIR set, install.sh wrote ~/.agents")
				}
			}
			if !strings.Contains(string(out), hint+"\n") {
				t.Fatalf("install.sh did not print the link hint")
			}
			skillTarget, err := filepath.EvalSymlinks(skillDir)
			if err != nil {
				t.Fatal(err)
			}
			manual := `skill: manual link: test -d "$HOME/.claude" && mkdir -p "$HOME/.claude/skills" && ln -s '` + skillTarget + `' "$HOME/.claude/skills/taskr"`
			if !strings.Contains(string(out), manual+"\n") {
				t.Fatalf("install.sh did not print a manual link for resolved skill path %q", skillTarget)
			}
		})
	}
}

func TestInstallSkillLinks(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("install.sh supports darwin and linux only")
	}
	host := "taskr-" + runtime.GOOS + "-" + runtime.GOARCH
	script, err := filepath.Abs("install.sh")
	if err != nil {
		t.Fatal(err)
	}
	skill := skillTarball(t)
	runInstall := func(t *testing.T, home, skillPath string, noSkill bool) string {
		t.Helper()
		bin := filepath.Join(home, "bin")
		if err := os.MkdirAll(bin, 0o755); err != nil {
			t.Fatal(err)
		}
		write := func(name string, data []byte, mode os.FileMode) {
			t.Helper()
			if err := os.WriteFile(filepath.Join(bin, name), data, mode); err != nil {
				t.Fatal(err)
			}
		}
		write("curl", []byte(fakeCurl), 0o755)
		write("gh", []byte("#!/bin/sh\nexit 1\n"), 0o755)
		write("herdr", []byte("#!/bin/sh\nexit 99\n"), 0o755)
		names := []string{host, "SHA256SUMS"}
		if !noSkill {
			names = append(names, "skill.tar.gz")
		}
		write("release.json", []byte(releaseJSON(names, false, "olafurns7")), 0o644)
		body := []byte("#!/bin/sh\necho link-test\n")
		write("asset.101", body, 0o644)
		sums := fmt.Sprintf("%x  %s\n", sha256.Sum256(body), host)
		if !noSkill {
			sums += fmt.Sprintf("%x  skill.tar.gz\n", sha256.Sum256(skill))
			write("asset.103", skill, 0o644)
		}
		write("asset.102", []byte(sums), 0o644)
		env := []string{
			"PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"),
			"HOME=" + home, "GH_TOKEN=test-token", "TASKR_LINK_SKILLS=1", "TASKR_NO_PLUGIN=1",
			"TASKR_INSTALL_DIR=" + filepath.Join(home, "installed bin"), "TASKR_SKILL_DIR=" + skillPath,
		}
		if noSkill {
			env = append(env, "TASKR_NO_SKILL=1")
		}
		cmd := exec.Command("sh", script)
		cmd.Dir, cmd.Env = home, env
		out, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("install.sh: %v\n%s", err, out)
		}
		return string(out)
	}
	for _, tc := range []struct {
		name, conflict  string
		noSkill, repeat bool
	}{
		{name: "relative-equivalent-link-shared-skills-directory", repeat: true},
		{name: "real-directory-conflict", conflict: "directory"},
		{name: "file-conflict", conflict: "file"},
		{name: "different-symlink-conflict", conflict: "different-symlink"},
		{name: "dangling-symlink-conflict", conflict: "dangling-symlink"},
		{name: "no-skill-bypasses-links", noSkill: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			home := t.TempDir()
			skillPath := "custom skills/taskr"
			claude := filepath.Join(home, ".claude")
			opencode := filepath.Join(home, ".config", "opencode")
			claudeLink := filepath.Join(claude, "skills", "taskr")
			if err := os.MkdirAll(claude, 0o755); err != nil {
				t.Fatal(err)
			}
			var oldLink string
			switch {
			case tc.repeat:
				if err := os.MkdirAll(filepath.Dir(claudeLink), 0o755); err != nil {
					t.Fatal(err)
				}
				oldLink = filepath.Join("..", "..", skillPath)
				if err := os.Symlink(oldLink, claudeLink); err != nil {
					t.Fatal(err)
				}
				shared := filepath.Join(home, "shared skills")
				if err := os.MkdirAll(shared, 0o755); err != nil {
					t.Fatal(err)
				}
				if err := os.MkdirAll(opencode, 0o755); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(shared, filepath.Join(opencode, "skills")); err != nil {
					t.Fatal(err)
				}
			case tc.conflict != "":
				if err := os.MkdirAll(filepath.Dir(claudeLink), 0o755); err != nil {
					t.Fatal(err)
				}
				codex := filepath.Join(home, ".codex")
				if err := os.MkdirAll(codex, 0o755); err != nil {
					t.Fatal(err)
				}
				if err := os.MkdirAll(opencode, 0o755); err != nil {
					t.Fatal(err)
				}
				switch tc.conflict {
				case "directory":
					if err := os.Mkdir(claudeLink, 0o755); err != nil {
						t.Fatal(err)
					}
					if err := os.WriteFile(filepath.Join(claudeLink, "keep"), []byte("kept"), 0o644); err != nil {
						t.Fatal(err)
					}
				case "file":
					if err := os.WriteFile(claudeLink, []byte("kept"), 0o644); err != nil {
						t.Fatal(err)
					}
				case "different-symlink":
					other := filepath.Join(home, "other skill")
					if err := os.MkdirAll(other, 0o755); err != nil {
						t.Fatal(err)
					}
					oldLink = other
					if err := os.Symlink(oldLink, claudeLink); err != nil {
						t.Fatal(err)
					}
				case "dangling-symlink":
					oldLink = "missing skill"
					if err := os.Symlink(oldLink, claudeLink); err != nil {
						t.Fatal(err)
					}
				}
			}

			out := runInstall(t, home, skillPath, tc.noSkill)
			target := filepath.Join(home, skillPath)
			if tc.noSkill {
				if _, err := os.Lstat(target); !os.IsNotExist(err) {
					t.Fatalf("TASKR_NO_SKILL=1 created %s: %v", target, err)
				}
				if _, err := os.Lstat(filepath.Join(claude, "skills")); !os.IsNotExist(err) {
					t.Fatalf("TASKR_NO_SKILL=1 created agent skills dir: %v", err)
				}
				if strings.Contains(out, "skill: linked:") {
					t.Fatalf("TASKR_NO_SKILL=1 created a link: %s", out)
				}
				return
			}
			canonical, err := filepath.EvalSymlinks(target)
			if err != nil {
				t.Fatal(err)
			}
			if tc.repeat {
				out = runInstall(t, home, skillPath, false)
				if got, err := os.Readlink(claudeLink); err != nil || got != oldLink {
					t.Fatalf("equivalent relative link = %q (%v), want %q", got, err, oldLink)
				}
				sharedLink := filepath.Join(home, "shared skills", "taskr")
				opencodeLink := filepath.Join(opencode, "skills", "taskr")
				if got, err := os.Readlink(sharedLink); err != nil || got != canonical {
					t.Fatalf("shared skills link = %q (%v), want %q", got, err, canonical)
				}
				if !strings.Contains(out, "skill: already linked: "+claudeLink) || !strings.Contains(out, "skill: already linked: "+opencodeLink) {
					t.Fatalf("repeat install did not recognize equivalent links:\n%s", out)
				}
				if _, err := os.Lstat(filepath.Join(home, ".codex")); !os.IsNotExist(err) {
					t.Fatalf("install.sh created an absent agent config: %v", err)
				}
			} else if tc.conflict != "" {
				if !strings.Contains(out, "skill: conflict: "+claudeLink) {
					t.Fatalf("conflict was not reported:\n%s", out)
				}
				info, err := os.Lstat(claudeLink)
				if err != nil {
					t.Fatal(err)
				}
				switch tc.conflict {
				case "directory":
					if !info.IsDir() {
						t.Fatalf("directory conflict changed to %v", info.Mode())
					}
					if b, err := os.ReadFile(filepath.Join(claudeLink, "keep")); err != nil || string(b) != "kept" {
						t.Fatalf("directory conflict contents = %q (%v)", b, err)
					}
				case "file":
					if info.Mode()&os.ModeSymlink != 0 || info.IsDir() {
						t.Fatalf("file conflict changed to %v", info.Mode())
					}
					if b, err := os.ReadFile(claudeLink); err != nil || string(b) != "kept" {
						t.Fatalf("file conflict contents = %q (%v)", b, err)
					}
				default:
					if info.Mode()&os.ModeSymlink == 0 {
						t.Fatalf("symlink conflict changed to %v", info.Mode())
					}
					if got, err := os.Readlink(claudeLink); err != nil || got != oldLink {
						t.Fatalf("symlink conflict target = %q (%v), want %q", got, err, oldLink)
					}
				}
				for _, agentLink := range []string{
					filepath.Join(home, ".codex", "skills", "taskr"),
					filepath.Join(opencode, "skills", "taskr"),
				} {
					if got, err := os.Readlink(agentLink); err != nil || got != canonical {
						t.Fatalf("other agent link %s = %q (%v), want %q", agentLink, got, err, canonical)
					}
				}
			}
		})
	}
}

func TestInstallRunbookBootstrap(t *testing.T) {
	readme, err := os.ReadFile("README.md")
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(readme), "https://raw.githubusercontent.com/olafurns7/herdr-taskr/master/docs/install.md") {
		t.Fatal("README is missing the raw install runbook URL")
	}
	runbook, err := os.ReadFile("docs/install.md")
	if err != nil {
		t.Fatal(err)
	}
	installParts := strings.SplitN(string(runbook), "## 2. Install\n", 2)
	if len(installParts) != 2 {
		t.Fatal("runbook is missing the install heading")
	}
	sectionParts := strings.SplitN(installParts[1], "\n## 3. Set PATH\n", 2)
	if len(sectionParts) != 2 {
		t.Fatal("runbook is missing the PATH heading")
	}
	parts := strings.Split(sectionParts[0], "```sh\n")
	if len(parts) != 3 {
		t.Fatal("install section must contain its curl and gh api snippets")
	}
	snippet := func(part string) string { return strings.SplitN(part, "\n```", 2)[0] }
	for _, tc := range []struct {
		name, method string
		fail         bool
	}{
		{name: "curl-success", method: "curl"},
		{name: "curl-partial-failure", method: "curl", fail: true},
		{name: "gh-api-success", method: "gh-api"},
		{name: "gh-api-partial-failure", method: "gh-api", fail: true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			home := t.TempDir()
			bin := filepath.Join(home, "bin")
			if err := os.MkdirAll(bin, 0o755); err != nil {
				t.Fatal(err)
			}
			write := func(name, body string) {
				t.Helper()
				if err := os.WriteFile(filepath.Join(bin, name), []byte(body), 0o755); err != nil {
					t.Fatal(err)
				}
			}
			installer := `#!/bin/sh
[ "$TASKR_LINK_SKILLS" = 1 ] || exit 78
printf installed > "$HOME/install-ran"
`
			if err := os.MkdirAll(filepath.Join(home, ".local", "bin"), 0o755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(home, ".local", "bin", "taskr"), []byte(`#!/bin/sh
[ "$1" = version ] || exit 79
printf version > "$HOME/version-ran"
printf 'vtest\n'
`), 0o755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(home, "full-installer.sh"), []byte(installer), 0o644); err != nil {
				t.Fatal(err)
			}
			write("gh", `#!/bin/sh
case $1 in
  auth)
    case $2 in
      login) exit 0 ;;
      token) printf 'test-token\n' ;;
      *) exit 2 ;;
    esac
    ;;
  api)
    printf '%s\n' "$@" > "$HOME/gh-args"
    if [ "$FAIL_DOWNLOAD" = 1 ]; then
      printf '%s\n' '#!/bin/sh' 'printf partial > "$HOME/partial-ran"'
      exit 1
    fi
    cat "$HOME/full-installer.sh"
    ;;
  *) exit 2 ;;
esac
`)
			write("curl", `#!/bin/sh
printf '%s\n' "$@" > "$HOME/curl-args"
out=
while [ "$#" -gt 0 ]; do
  case $1 in
    --output|-o) out=$2; shift 2 ;;
    *) shift ;;
  esac
done
cat > "$HOME/curl-stdin"
if [ "$FAIL_DOWNLOAD" = 1 ]; then
  printf '%s\n' '#!/bin/sh' 'printf partial > "$HOME/partial-ran"' > "$out"
  exit 22
fi
cat "$HOME/full-installer.sh" > "$out"
`)
			write("sh", `#!/bin/sh
printf called > "$HOME/sh-ran"
exec /bin/sh "$@"
`)
			block := parts[1]
			if tc.method == "gh-api" {
				block = parts[2]
			}
			fail := "0"
			if tc.fail {
				fail = "1"
			}
			cmd := exec.Command("/bin/sh", "-c", snippet(block))
			cmd.Dir = home
			cmd.Env = []string{"HOME=" + home, "PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"), "FAIL_DOWNLOAD=" + fail}
			out, runErr := cmd.CombinedOutput()
			if tc.fail {
				if runErr == nil {
					t.Fatalf("partial download unexpectedly succeeded: %s", out)
				}
				for _, marker := range []string{"sh-ran", "install-ran", "version-ran", "partial-ran"} {
					if _, err := os.Stat(filepath.Join(home, marker)); !os.IsNotExist(err) {
						t.Fatalf("failed download reached %s: %v", marker, err)
					}
				}
				if b, err := os.ReadFile(filepath.Join(home, "taskr-install.sh")); err != nil || !strings.HasPrefix(string(b), "#!/bin/sh\n") {
					t.Fatalf("partial download fixture = %q (%v), want executable text", b, err)
				}
				return
			}
			if runErr != nil {
				t.Fatalf("bootstrap: %v\n%s", runErr, out)
			}
			for _, marker := range []string{"sh-ran", "install-ran", "version-ran"} {
				if _, err := os.Stat(filepath.Join(home, marker)); err != nil {
					t.Fatalf("bootstrap did not create %s: %v", marker, err)
				}
			}
			if !strings.Contains(string(out), "vtest") {
				t.Fatalf("taskr version output = %q", out)
			}
			if tc.method == "curl" {
				args, err := os.ReadFile(filepath.Join(home, "curl-args"))
				if err != nil || !strings.Contains(string(args), "https://github.com/olafurns7/herdr-taskr/releases/latest/download/install.sh") {
					t.Fatalf("curl args = %q (%v), want public installer URL", args, err)
				}
				stdin, err := os.ReadFile(filepath.Join(home, "curl-stdin"))
				if err != nil || strings.Contains(string(args), "Authorization") || strings.Contains(string(args), "test-token") || strings.Contains(string(stdin), "Authorization") || strings.Contains(string(stdin), "test-token") {
					t.Fatalf("curl args/stdin = %q / %q (%v), want anonymous request", args, stdin, err)
				}
			} else if args, err := os.ReadFile(filepath.Join(home, "gh-args")); err != nil || !strings.Contains(string(args), "application/vnd.github.raw+json") {
				t.Fatalf("gh api args = %q (%v)", args, err)
			}
		})
	}
}

// Deliberately restrictive archive modes: installer must repair all 755/644.
func skillTarball(t *testing.T) []byte {
	t.Helper()
	var buf bytes.Buffer
	gz := gzip.NewWriter(&buf)
	tw := tar.NewWriter(gz)
	for name, body := range map[string]string{"SKILL.md": "# taskr skill\n", "references/format.md": "format reference\n", "references/orchestrator.md": "orchestrator reference\n"} {
		if err := tw.WriteHeader(&tar.Header{Name: name, Mode: 0o600, Size: int64(len(body)), Typeflag: tar.TypeReg}); err != nil {
			t.Fatal(err)
		}
		if _, err := tw.Write([]byte(body)); err != nil {
			t.Fatal(err)
		}
	}
	if err := tw.Close(); err != nil {
		t.Fatal(err)
	}
	if err := gz.Close(); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

func TestInstallSkillGH(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Skip("unsupported install host")
	}
	for _, corrupt := range []bool{false, true} {
		t.Run(fmt.Sprint(corrupt), func(t *testing.T) {
			dir := t.TempDir()
			bin := filepath.Join(dir, "bin")
			os.MkdirAll(bin, 0o755)
			host := "taskr-" + runtime.GOOS + "-" + runtime.GOARCH
			body := []byte("#!/bin/sh\necho ok\n")
			skill := skillTarball(t)
			sums := fmt.Sprintf("%x  %s\n%x  skill.tar.gz\n", sha256.Sum256(body), host, sha256.Sum256(skill))
			if corrupt {
				skill = append(skill, 'x')
			}
			for name, data := range map[string][]byte{host: body, "skill.tar.gz": skill, "SHA256SUMS": []byte(sums)} {
				os.WriteFile(filepath.Join(bin, name), data, 0o644)
			}
			gh := `#!/bin/sh
d="$(dirname "$0")"
[ "$1 $2" = "auth status" ] && exit 0
[ "$1 $2" = "release download" ] || exit 1
while [ "$#" -gt 0 ]; do
 case $1 in
  --pattern) assets="$assets $2"; shift ;;
  --dir) dest=$2; shift ;;
 esac
 shift
done
for asset in $assets; do cp "$d/$asset" "$dest/$asset" || exit 1; done
`
			os.WriteFile(filepath.Join(bin, "gh"), []byte(gh), 0o755)
			skillDir := filepath.Join(dir, "custom", "skill")
			installDir := filepath.Join(dir, "inst")
			cmd := exec.Command("sh", "install.sh")
			cmd.Env = []string{"PATH=" + bin + string(os.PathListSeparator) + os.Getenv("PATH"), "HOME=" + dir, "TASKR_INSTALL_DIR=" + installDir, "TASKR_SKILL_DIR=" + skillDir, "TASKR_NO_PLUGIN=1"}
			out, err := cmd.CombinedOutput()
			if corrupt {
				if err == nil || !strings.Contains(string(out), "checksum verification failed: skill.tar.gz") {
					t.Fatalf("corrupt skill: %v %s", err, out)
				}
				if _, err := os.Stat(filepath.Join(installDir, "taskr")); err == nil {
					t.Fatal("installed binary before skill validation")
				}
				return
			}
			if err != nil {
				t.Fatalf("gh install: %v %s", err, out)
			}
			for _, name := range []string{"format.md", "orchestrator.md"} {
				path := filepath.Join(skillDir, "references", name)
				info, err := os.Stat(path)
				if err != nil || info.Mode() != 0o644 {
					t.Fatalf("reference mode: %v %v", info, err)
				}
				b, _ := os.ReadFile(path)
				if string(b) != strings.TrimSuffix(name, ".md")+" reference\n" {
					t.Fatalf("reference: %q", b)
				}
			}
		})
	}
}
