// Command golden writes the golden data under tests/data.
//
// Every expected value in those files is produced here by the Go standard
// library -- encoding/json, strconv and time -- which is what the Go client
// (github.com/librespeed/speedtest-cli) renders its reports with.
// tests/golden.rs feeds the same inputs to this client and compares the
// results byte for byte.
//
// Usage, from the repository root:
//
//	go run tools/golden/main.go tests/data
//
// The data was generated with go1.26.5, and every file records the version
// that wrote it. The version matters in one place: encoding/json writes U+0008
// and U+000C as \b and \f since Go 1.22, and as \u0008 and \u000c before. The
// Go client's go.mod requires Go 1.25, so the short forms are what it prints.
//
// The report types below are copies of the Go client's, taken at commit
// b660d1e: report/json.go, defs/defs.go and output/stream.go. Only the field
// order, the types and the tags matter to the encoder.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"time"
)

// JSONReport is report.JSONReport.
type JSONReport struct {
	Timestamp     time.Time  `json:"timestamp"`
	Server        Server     `json:"server"`
	Client        Client     `json:"client"`
	BytesSent     uint64     `json:"bytes_sent"`
	BytesReceived uint64     `json:"bytes_received"`
	Ping          float64    `json:"ping"`
	Jitter        float64    `json:"jitter"`
	Upload        float64    `json:"upload"`
	Download      float64    `json:"download"`
	Share         string     `json:"share"`
	TLS           *TLSReport `json:"tls,omitempty"`
}

// TLSReport is report.TLSReport.
type TLSReport struct {
	Version string `json:"version"`
	Cipher  string `json:"cipher"`
}

// Server is report.Server.
type Server struct {
	Name string `json:"name"`
	URL  string `json:"url"`
}

// Client is report.Client.
type Client struct {
	IPInfoResponse
}

// IPInfoResponse is defs.IPInfoResponse.
type IPInfoResponse struct {
	IP           string `json:"ip"`
	Hostname     string `json:"hostname"`
	City         string `json:"city"`
	Region       string `json:"region"`
	Country      string `json:"country"`
	Location     string `json:"loc"`
	Organization string `json:"org"`
	Postal       string `json:"postal"`
	Timezone     string `json:"timezone"`
	Readme       string `json:"readme,omitempty"`
}

// ProgressEvent is output.ProgressEvent.
type ProgressEvent struct {
	Event    string  `json:"event"`
	Phase    string  `json:"phase"`
	Seconds  float64 `json:"seconds"`
	Mbps     float64 `json:"mbps"`
	Progress int     `json:"progress"`
}

// An instant is a timestamp the way both languages can build it exactly.
type instant struct {
	Sec    int64 `json:"sec"`
	Nsec   int64 `json:"nsec"`
	Offset int   `json:"offset"`
}

func (i instant) time() time.Time {
	return time.Unix(i.Sec, i.Nsec).In(time.FixedZone("", i.Offset))
}

// A reportIn is what the Rust test builds one report from. Floats travel as
// the hexadecimal bits of the float64, so that no decimal parser stands
// between the two sides.
type reportIn struct {
	At            instant    `json:"at"`
	Name          string     `json:"name"`
	URL           string     `json:"url"`
	Client        [10]string `json:"client"`
	BytesSent     uint64     `json:"bytes_sent"`
	BytesReceived uint64     `json:"bytes_received"`
	Ping          string     `json:"ping"`
	Jitter        string     `json:"jitter"`
	Upload        string     `json:"upload"`
	Download      string     `json:"download"`
	Share         string     `json:"share"`
	TLS           *[2]string `json:"tls"`
}

type progressIn struct {
	Phase    string `json:"phase"`
	Seconds  string `json:"seconds"`
	Mbps     string `json:"mbps"`
	Progress int    `json:"progress"`
}

// A caseIn is one line of go_json.tsv: a list of reports, or one progress
// event.
type caseIn struct {
	Reports  []reportIn  `json:"reports"`
	Progress *progressIn `json:"progress,omitempty"`
}

func bits(f float64) string { return fmt.Sprintf("%016x", math.Float64bits(f)) }

func unbits(s string) float64 {
	u, err := strconv.ParseUint(s, 16, 64)
	if err != nil {
		panic(err)
	}
	return math.Float64frombits(u)
}

func (r reportIn) report() JSONReport {
	rep := JSONReport{
		Timestamp: r.At.time(),
		Server:    Server{Name: r.Name, URL: r.URL},
		Client: Client{IPInfoResponse{
			IP: r.Client[0], Hostname: r.Client[1], City: r.Client[2],
			Region: r.Client[3], Country: r.Client[4], Location: r.Client[5],
			Organization: r.Client[6], Postal: r.Client[7],
			Timezone: r.Client[8], Readme: r.Client[9],
		}},
		BytesSent:     r.BytesSent,
		BytesReceived: r.BytesReceived,
		Ping:          unbits(r.Ping),
		Jitter:        unbits(r.Jitter),
		Upload:        unbits(r.Upload),
		Download:      unbits(r.Download),
		Share:         r.Share,
	}
	if r.TLS != nil {
		rep.TLS = &TLSReport{Version: r.TLS[0], Cipher: r.TLS[1]}
	}
	return rep
}

// expected marshals a case exactly as the Go client does: speedtest/helper.go
// declares a nil slice of reports and marshals a pointer to it, and
// output/stream.go marshals the event value.
func (c caseIn) expected() []byte {
	var v interface{}
	if c.Progress != nil {
		v = ProgressEvent{
			Event:    "progress",
			Phase:    c.Progress.Phase,
			Seconds:  unbits(c.Progress.Seconds),
			Mbps:     unbits(c.Progress.Mbps),
			Progress: c.Progress.Progress,
		}
	} else {
		var reps []JSONReport
		for _, r := range c.Reports {
			reps = append(reps, r.report())
		}
		v = &reps
	}
	b, err := json.Marshal(v)
	if err != nil {
		panic(err)
	}
	return b
}

// splitmix64 keeps the sampled part of the corpus the same on every run and
// under every Go release.
type splitmix64 uint64

func (s *splitmix64) next() uint64 {
	*s += 0x9e3779b97f4a7c15
	z := uint64(*s)
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

// unit returns a float64 in [0, 1).
func (s *splitmix64) unit() float64 { return float64(s.next()>>11) / (1 << 53) }

// round2 and round1 are the client's own rounding: speedtest/helper.go for the
// reports, defs/server.go for the progress events.
func round2(x float64) float64 { return math.Round(x*100) / 100 }
func round1(x float64) float64 { return math.Round(x*10) / 10 }

// floats is every float64 the corpus formats, each of them once.
func floats() []float64 {
	// Added at run time: as constants, 0.1 + 0.2 is exactly 0.3.
	tenth, fifth := 0.1, 0.2
	edge := []float64{
		0, math.Copysign(0, -1), 1, 0.5, 0.1, 0.2, tenth + fifth, 0.3, 1.0 / 3,
		0.01, 0.07, 0.29, 0.57, 1.15, 4.35, 99.99, 100, 100.1, 1000000,
		6.36, 0.82, 228.39, 553.97, 941.0, 9403.51, 100000.25,
		// The thresholds of encoding/json: 'e' below 1e-6 and from 1e21.
		1e-4, 1e-5, 1.5e-5, 9.999999e-6, 1e-6, 1.000001e-6, 1.5e-6,
		math.Nextafter(1e-6, 0), math.Nextafter(1e-6, 1), 9.99e-7, 1e-7,
		1.5e-7, 1.234e-10, 1e-9, 1e-10, 1e-99, 1e-100, 1.5e-100,
		1e20, 1.5e20, 123456789012345680000, math.Nextafter(1e21, 0), 1e21,
		math.Nextafter(1e21, math.Inf(1)), 1.5e21, 1e22, 1e23, 1e99, 1e100,
		1.5e100, 1e300,
		// Where a float64 stops holding every integer, and around it.
		1e14, 1e15 - 1, 1e15, 1e15 + 1, 1e15 + 0.5, 999999999999999.9,
		1 << 53, 1<<53 - 1, 1<<53 + 2, 1e16, 1e17, 1e18, 1e19,
		9223372036854775807, 18446744073709551615,
		// The ends of the range.
		math.MaxFloat64, math.Nextafter(math.MaxFloat64, 0),
		2.2250738585072014e-308, 2.225073858507201e-308,
		math.SmallestNonzeroFloat64, 2 * math.SmallestNonzeroFloat64,
		1e-323, 1e-310,
	}
	var out []float64
	for _, f := range edge {
		out = append(out, f)
		if f > 0 {
			out = append(out, -f)
		}
	}

	// What the client can print: every hundredth up to 3, every tenth up to
	// 12, and rounded samples across the magnitudes a rate or a latency has.
	for k := 0; k <= 300; k++ {
		out = append(out, round2(float64(k)/100))
	}
	for k := 0; k <= 120; k++ {
		out = append(out, round1(float64(k)/10))
	}
	rng := splitmix64(1)
	for _, scale := range []float64{1, 10, 100, 1000, 10000, 100000, 1e7, 1e9, 1e12} {
		for i := 0; i < 60; i++ {
			out = append(out, round2(rng.unit()*scale))
		}
	}

	// Arbitrary bit patterns, for the digits and the exponents in between.
	for i := 0; i < 300; i++ {
		f := math.Float64frombits(rng.next())
		if !math.IsNaN(f) && !math.IsInf(f, 0) {
			out = append(out, f)
		}
	}

	seen := make(map[uint64]bool)
	var once []float64
	for _, f := range out {
		if b := math.Float64bits(f); !seen[b] {
			seen[b] = true
			once = append(once, f)
		}
	}
	return once
}

// jsonFloat is the number as encoding/json writes it, taken from the encoder
// itself rather than from a copy of its rule.
func jsonFloat(f float64) string {
	b, err := json.Marshal(f)
	if err != nil {
		panic(err)
	}
	return string(b)
}

// csvFloat is the number as the Go client's CSV carries it: gocsv formats a
// float64 field with strconv.FormatFloat(f, 'f', -1, 64).
func csvFloat(f float64) string { return strconv.FormatFloat(f, 'f', -1, 64) }

func writeFloats(w *bytes.Buffer) {
	fmt.Fprintln(w, "# float64 bits <TAB> encoding/json <TAB> strconv 'f', which gocsv uses")
	for _, f := range floats() {
		fmt.Fprintf(w, "%s\t%s\t%s\n", bits(f), jsonFloat(f), csvFloat(f))
	}
}

func writeRounding(w *bytes.Buffer) {
	fmt.Fprintln(w, "# float64 bits <TAB> bits of math.Round(x*100)/100")
	rng := splitmix64(2)
	xs := []float64{0, 0.004, 0.005, 0.015, 0.025, 0.045, 1.005, 2.675, 5.455, 199.804, 1e15, 1e300}
	for _, scale := range []float64{1, 100, 10000, 1e6, 1e9} {
		for i := 0; i < 60; i++ {
			xs = append(xs, rng.unit()*scale)
		}
	}
	for _, x := range xs {
		fmt.Fprintf(w, "%s\t%s\n", bits(x), bits(round2(x)))
	}
}

func instants() []instant {
	const base = 1786023696 // 2026-08-06T13:41:36Z
	var out []instant
	// Every length of fraction, with and without zeros inside it.
	for _, ns := range []int64{
		0, 1, 10, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000,
		900000000, 990000000, 999000000, 999900000, 999990000, 999999000,
		999999900, 999999990, 999999999, 123456789, 120000000, 100200300,
		1001, 67293000, 411388000, 380000000, 20091000, 500000000,
	} {
		out = append(out, instant{base, ns, 7200}, instant{base, ns, 0})
	}
	// Offsets: zero, whole hours, half and quarter hours, the extremes in
	// use, and ones a minute cannot express, which Go truncates.
	for _, off := range []int{
		0, 3600, -3600, 7200, 19800, -19800, 20700, 45900, 50400, -43200,
		-34200, 60, -60, 59, -59, 1, -1, 30, -30, 61, -61, 90, -90, 3599,
		-3599, 3661, -3661, 86399, -86399,
	} {
		out = append(out, instant{base, 0, off}, instant{base, 123000000, off})
	}
	// Year, month and day boundaries, either side of them, and the far ends
	// of what RFC 3339 can carry.
	for _, sec := range []int64{
		-62167219200, -62135596800, -1, 0, 1, 951782399, 951782400,
		946684799, 946684800, 1483228799, 1483228800, 1798761599,
		1798761600, 2147483647, 2147483648, 4102444800, 253402300799,
	} {
		out = append(out, instant{sec, 0, 0}, instant{sec, 999999999, 0})
		if sec > -62167219200 && sec < 253402300799 {
			out = append(out, instant{sec, 500000000, 3600}, instant{sec, 5000, -3600})
		}
	}
	return out
}

func writeTimestamps(w *bytes.Buffer) {
	fmt.Fprintln(w, "# unix seconds <TAB> nanoseconds <TAB> offset east of UTC in seconds <TAB> time.Time.MarshalText")
	for _, i := range instants() {
		b, err := i.time().MarshalText()
		if err != nil {
			panic(err)
		}
		fmt.Fprintf(w, "%d\t%d\t%d\t%s\n", i.Sec, i.Nsec, i.Offset, b)
	}
}

// plainTexts are left alone by every sanitizer, so they can stand in any
// field of a report: this client cleans what came off the wire before it
// reports it, and the Go client does not.
var plainTexts = []string{
	"",
	"Prague, Czech Republic (CESNET)",
	"https://speedtest.cesnet.cz/backend/",
	"AT&T <Lab>",
	"http://x/?a=1&b=2&c=<3>",
	"</script><script>alert(1)</script>",
	`say "hi"`,
	`back\slash \\ \n \u0041 \`,
	`"`,
	`\`,
	"/", "a/b", "'", "&&", "<<>>", "&amp;", "\\u003c",
	" !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~",
	"Příliš žluťoučký kůň úpěl ďábelské ódy",
	"東京 (さくらインターネット)",
	"Київ, Україна",
	"\U0001F600 astral \U0001D11E plane \U0010FFFD",
	"e\u0301 combining", "\u00a0no-break\u00a0space", "\ufffd replacement",
	"203.0.113.9", "2001:db8::1", "AS64496 Example <&> Net", "50.0880,14.4208",
	"Europe/Prague", "110 00",
}

// rawTexts go where neither client cleans: the fields this client fills in
// itself. They are here for the encoder, which has to get them right for
// whatever field comes next.
func rawTexts() []string {
	var out []string
	var c0 string
	for r := rune(0); r < 0x20; r++ {
		out = append(out, "a"+string(r)+"b")
		c0 += string(r)
	}
	return append(out,
		c0,
		"del\x7fete", "c1\u0080\u0085\u009b\u009f",
		"line\u2028sep", "para\u2029sep", "\u2028", "\u2029\u2029",
		"near\u2027\u202a\u202e\u2030", "\u200b\u200d\u00ad\ufeff",
		"tags\U000E0001\U000E0041\U000E007F", "\ufffe\uffff", "\U0010FFFF",
		"\x1b[31mred\x1b[0m <&> \u2028 \"q\" \\ /",
		"\b\f\n\r\t", "\x00", "\x1f",
	)
}

func cases() []caseIn {
	at := instant{1786023696, 67293000, 7200}
	client := [10]string{
		"203.0.113.9", "host.example", "Prague", "Hlavní město Praha", "CZ",
		"50.0880,14.4208", "AS64496 Example Net", "110 00", "Europe/Prague", "",
	}
	typical := reportIn{
		At: at, Name: "Prague, Czech Republic (CESNET)",
		URL: "https://speedtest.cesnet.cz/backend/", Client: client,
		BytesSent: 1038090240, BytesReceived: 428146688,
		Ping: bits(6.36), Jitter: bits(0.82), Upload: bits(553.97),
		Download: bits(228.39), Share: "https://librespeed.org/results/?id=abc",
		TLS: &[2]string{"TLS 1.3", "TLS_AES_128_GCM_SHA256"},
	}

	var out []caseIn

	// No report at all is null, not an empty array.
	out = append(out, caseIn{})
	out = append(out, caseIn{Reports: []reportIn{typical}})

	// Over plain HTTP there is no tls member; a readme appears only when set.
	plain := typical
	plain.TLS = nil
	plain.URL = "http://speedtest.cesnet.cz/backend/"
	out = append(out, caseIn{Reports: []reportIn{plain}})
	readme := typical
	readme.Client[9] = "https://ipinfo.io/missingauth"
	out = append(out, caseIn{Reports: []reportIn{readme}})
	out = append(out, caseIn{Reports: []reportIn{typical, plain, readme}})

	// The zero report.
	zero := reportIn{Ping: bits(0), Jitter: bits(0), Upload: bits(0), Download: bits(0)}
	out = append(out, caseIn{Reports: []reportIn{zero}})

	// One text in every field it can stand in.
	for _, s := range plainTexts {
		r := typical
		r.Name, r.URL, r.Share = s, s, s
		for i := range r.Client {
			r.Client[i] = s
		}
		r.TLS = &[2]string{s, s}
		out = append(out, caseIn{Reports: []reportIn{r}})
	}
	for _, s := range rawTexts() {
		r := typical
		r.TLS = &[2]string{s, s}
		out = append(out, caseIn{Reports: []reportIn{r}})
	}

	// Numbers: whole ones, rounded ones, the ends of the counters, and the
	// values where the notation changes.
	for _, f := range [][4]float64{
		{0, 0, 0, 0}, {1, 2, 3, 4}, {100, 10, 1000, 10000},
		{6.3, 0.8, 553.9, 228.3}, {0.01, 0.07, 0.29, 0.57},
		{12.5, 0.25, 9403.51, 100000.25}, {1e15, 1e16, 1e20, 1e21},
		{1e-5, 1e-6, 1e-7, 1.5e-9}, {math.Copysign(0, -1), -1.5, -1e21, -1e-7},
		{math.MaxFloat64, math.SmallestNonzeroFloat64, 1 << 53, 0.1 + 0.2},
	} {
		r := typical
		r.Ping, r.Jitter, r.Upload, r.Download = bits(f[0]), bits(f[1]), bits(f[2]), bits(f[3])
		out = append(out, caseIn{Reports: []reportIn{r}})
	}
	for _, n := range []uint64{0, 1, 1 << 32, 1 << 53, 1<<53 + 1, 1<<63 - 1, 1 << 63, 1<<64 - 1} {
		r := typical
		r.BytesSent, r.BytesReceived = n, n
		out = append(out, caseIn{Reports: []reportIn{r}})
	}

	// Timestamps inside a report, quotes and all.
	for _, i := range []instant{
		{1786023696, 0, 0}, {1786023696, 0, 7200}, {1786023696, 500000000, -19800},
		{1786023696, 999999999, 20700}, {1798761599, 999999999, 3600}, {0, 1, 0},
	} {
		r := typical
		r.At = i
		out = append(out, caseIn{Reports: []reportIn{r}})
	}

	// Progress events: a whole number has no fraction and no exponent.
	for _, p := range []struct {
		phase         string
		seconds, mbps float64
		progress      int
	}{
		{"download", 1, 94.27, 6}, {"download", 2, 100, 13}, {"download", 3.1, 0, 20},
		{"upload", 14.9, 941.5, 99}, {"upload", 15, 9403.51, 100}, {"upload", 0.1, 0.01, 0},
		{"download", 1000000, 1e21, 100}, {"download", 0.3, 1e-7, 1},
	} {
		out = append(out, caseIn{Progress: &progressIn{p.phase, bits(p.seconds), bits(p.mbps), p.progress}})
	}
	return out
}

func writeJSON(w *bytes.Buffer) {
	fmt.Fprintln(w, "# the input, as JSON <TAB> what encoding/json makes of it")
	for _, c := range cases() {
		in, err := json.Marshal(c)
		if err != nil {
			panic(err)
		}
		fmt.Fprintf(w, "%s\t%s\n", in, c.expected())
	}
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: go run tools/golden/main.go tests/data")
		os.Exit(2)
	}
	for name, write := range map[string]func(*bytes.Buffer){
		"go_floats.tsv":     writeFloats,
		"go_rounding.tsv":   writeRounding,
		"go_timestamps.tsv": writeTimestamps,
		"go_json.tsv":       writeJSON,
	} {
		var w bytes.Buffer
		fmt.Fprintf(&w, "# Generated by tools/golden/main.go with %s. Do not edit.\n", runtime.Version())
		write(&w)
		if err := os.WriteFile(filepath.Join(os.Args[1], name), w.Bytes(), 0o644); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	}
}
