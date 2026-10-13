// WeftOS chief-of-staff glance panel (ADR-119 S4).
// Waveshare 2.13" HAT Rev 2.1 (glass V4, 250x122) on an ESP32-S3-N16R8.
// Reads newline-delimited JSON on USB serial (115200):
//   {"v":1,"state":"idle|listening|thinking|speaking|blocked","task":"...",
//    "next":"...","alert":"...","model":"...","ts":"HH:MM"}
// and draws it. See README.md.
#include <SPI.h>
#include <GxEPD2_BW.h>
#include <gdey/GxEPD2_213_GDEY0213B74.h>
#include <Fonts/FreeSans9pt7b.h>
#include <Fonts/FreeSansBold9pt7b.h>
#include <Fonts/FreeSansBold12pt7b.h>

static const int EPD_CS = 10, EPD_DC = 9, EPD_RST = 8, EPD_BUSY = 13;
static const int EPD_SCK = 12, EPD_MOSI = 11;  // MISO unused: GPIO13 is BUSY

GxEPD2_BW<GxEPD2_213_GDEY0213B74, GxEPD2_213_GDEY0213B74::HEIGHT> display(
    GxEPD2_213_GDEY0213B74(EPD_CS, EPD_DC, EPD_RST, EPD_BUSY));

static const int W = 250, H = 122;
static const int FULL_EVERY = 6;                     // partials between full refreshes
static const uint32_t FULL_MS = 30UL * 60 * 1000;    // or this long since the last one
static const uint32_t NOLINK_MS = 5UL * 60 * 1000;   // no valid line for this long
static const size_t RX_MAX = 640;

struct Glance {
  char state[16], task[128], next[128], alert[128], model[48], ts[8];
};
static Glance cur, shown;
static bool haveShown = false, noLink = false, shownNoLink = false;
static int partials = 0;
static uint32_t lastFull = 0, lastValid = 0;
static char line[RX_MAX + 1];
static size_t len = 0;
static bool overflow = false;

// Extract a flat string field. Handles \" \\ \n \/ and drops \uXXXX to '?'.
// Returns false when the key is absent or not a string.
static bool jstr(const char* js, const char* key, char* out, size_t cap) {
  char pat[24];
  snprintf(pat, sizeof pat, "\"%s\"", key);
  const char* p = strstr(js, pat);
  if (!p) return false;
  p += strlen(pat);
  while (*p == ' ' || *p == ':') p++;
  if (*p != '"') return false;
  p++;
  size_t n = 0;
  while (*p && *p != '"') {
    char c = *p++;
    if (c == '\\') {
      char e = *p++;
      if (!e) return false;
      if (e == 'u') { for (int i = 0; i < 4 && *p; i++) p++; c = '?'; }
      else if (e == 'n' || e == 't' || e == 'r') c = ' ';
      else c = e;
    }
    if (c < 32 || c > 126) c = '?';
    if (n + 1 < cap) out[n++] = c;
  }
  if (*p != '"') return false;  // unterminated: malformed
  out[n] = 0;
  return true;
}

static bool validState(const char* s) {
  static const char* k[] = {"idle", "listening", "thinking", "speaking", "blocked"};
  for (auto w : k) if (!strcmp(s, w)) return true;
  return false;
}

static bool parseLine(const char* js, Glance& g) {
  if (js[0] != '{') return false;
  if (!strstr(js, "\"v\":1") && !strstr(js, "\"v\": 1")) return false;
  Glance t = {};
  if (!jstr(js, "state", t.state, sizeof t.state) || !validState(t.state)) return false;
  jstr(js, "task", t.task, sizeof t.task);
  jstr(js, "next", t.next, sizeof t.next);
  jstr(js, "alert", t.alert, sizeof t.alert);
  jstr(js, "model", t.model, sizeof t.model);
  jstr(js, "ts", t.ts, sizeof t.ts);
  g = t;
  return true;
}

// Fit text to maxW pixels with the current font, ending in "..." when cut.
static void fit(const char* s, int maxW, char* out, size_t cap) {
  int16_t x1, y1; uint16_t w, h;
  strlcpy(out, s, cap);
  display.getTextBounds(out, 0, 0, &x1, &y1, &w, &h);
  if (w <= maxW) return;
  size_t n = strlen(out);
  while (n > 0) {
    n--;
    out[n] = 0;
    char tmp[cap + 4];
    snprintf(tmp, sizeof tmp, "%s...", out);
    display.getTextBounds(tmp, 0, 0, &x1, &y1, &w, &h);
    if (w <= maxW) { strlcpy(out, tmp, cap); return; }
  }
  strlcpy(out, "...", cap);
}

// Word-wrap into at most two lines of maxW pixels; the second is truncated.
static void wrap2(const char* s, int maxW, char* l1, char* l2, size_t cap) {
  int16_t x1, y1; uint16_t w, h;
  l1[0] = l2[0] = 0;
  char tmp[cap];
  size_t n = strlen(s), best = 0;
  for (size_t i = 1; i <= n; i++) {
    if (s[i] != ' ' && s[i] != 0) continue;
    memcpy(tmp, s, i); tmp[i] = 0;
    display.getTextBounds(tmp, 0, 0, &x1, &y1, &w, &h);
    if (w > maxW) break;
    best = i;
  }
  if (best == 0) {  // a single word wider than the line: hard cut
    fit(s, maxW, l1, cap);
    return;
  }
  memcpy(l1, s, best); l1[best] = 0;
  const char* rest = s + best;
  while (*rest == ' ') rest++;
  fit(rest, maxW, l2, cap);
}

static void drawGlyph(const char* st, int cx, int cy) {
  // 18 px glyph per state, drawn around (cx, cy).
  if (!strcmp(st, "idle")) { display.drawCircle(cx, cy, 8, GxEPD_BLACK); display.drawCircle(cx, cy, 7, GxEPD_BLACK); }
  else if (!strcmp(st, "listening")) { display.fillCircle(cx, cy, 8, GxEPD_BLACK); display.fillCircle(cx, cy, 3, GxEPD_WHITE); }
  else if (!strcmp(st, "thinking")) { for (int i = -1; i <= 1; i++) display.fillCircle(cx + i * 7, cy, 2, GxEPD_BLACK); }
  else if (!strcmp(st, "speaking")) { display.fillTriangle(cx - 8, cy, cx + 6, cy - 8, cx + 6, cy + 8, GxEPD_BLACK); }
  else { display.fillTriangle(cx, cy - 9, cx - 9, cy + 8, cx + 9, cy + 8, GxEPD_BLACK);
         display.fillRect(cx - 1, cy - 3, 3, 7, GxEPD_WHITE); }
}

static void render(const Glance& g, bool noLinkMode) {
  char buf[160], l1[160], l2[160];
  display.fillScreen(GxEPD_WHITE);
  display.setTextColor(GxEPD_BLACK);

  // Header: glyph, state word, clock.
  drawGlyph(noLinkMode ? "blocked" : g.state, 14, 13);
  display.setFont(&FreeSansBold12pt7b);
  display.setCursor(30, 21);
  const char* word = noLinkMode ? "no link" : g.state;
  strlcpy(buf, word, sizeof buf);
  for (char* c = buf; *c; c++) if (*c >= 'a' && *c <= 'z') *c -= 32;
  display.print(buf);
  display.setFont(&FreeSans9pt7b);
  if (!noLinkMode && g.ts[0]) {
    int16_t x1, y1; uint16_t w, h;
    display.getTextBounds(g.ts, 0, 0, &x1, &y1, &w, &h);
    display.setCursor(W - 4 - w, 19);
    display.print(g.ts);
  }
  display.drawFastHLine(0, 27, W, GxEPD_BLACK);

  if (!noLinkMode) {
    // Task (bold) and next step (wrapped to two lines).
    display.setFont(&FreeSansBold9pt7b);
    fit(g.task[0] ? g.task : "-", W - 8, buf, sizeof buf);
    display.setCursor(4, 45);
    display.print(buf);
    display.setFont(&FreeSans9pt7b);
    wrap2(g.next[0] ? g.next : "-", W - 8, l1, l2, sizeof l1);
    display.setCursor(4, 63); display.print(l1);
    display.setCursor(4, 80); display.print(l2);

    // Alert bar, inverted, only when present.
    if (g.alert[0]) {
      display.fillRect(0, 86, W, 20, GxEPD_BLACK);
      display.setTextColor(GxEPD_WHITE);
      display.setFont(&FreeSansBold9pt7b);
      fit(g.alert, W - 8, buf, sizeof buf);
      display.setCursor(4, 101);
      display.print(buf);
      display.setTextColor(GxEPD_BLACK);
    }
    // Footer: model.
    display.setFont();
    display.setTextSize(1);
    fit(g.model, W - 8, buf, sizeof buf);
    display.setCursor(4, 112);
    display.print(buf);
  } else {
    display.setFont(&FreeSans9pt7b);
    display.setCursor(4, 55); display.print("No data from the Mac");
    display.setCursor(4, 75); display.print("for 5 minutes.");
  }
}

static void draw(const Glance& g, bool noLinkMode) {
  uint32_t now = millis();
  int dl = (int)strlen(g.next) - (int)strlen(shown.next);
  bool bigChange = noLinkMode != shownNoLink || abs(dl) > 40;
  bool full = !haveShown || bigChange || partials >= FULL_EVERY || now - lastFull >= FULL_MS;
  // No hibernate/re-init between updates: the hardware reset on wake loses
  // the controller's previous-image RAM, which made partial updates overlay
  // the old text. powerOff() keeps RAM; the panel is bistable so it costs nothing.
  display.setRotation(1);
  if (full) display.setFullWindow(); else display.setPartialWindow(0, 0, W, H);
  display.firstPage();
  do { render(g, noLinkMode); } while (display.nextPage());
  display.powerOff();
  shownNoLink = noLinkMode;
  if (full) { partials = 0; lastFull = now; } else partials++;
  shown = g; haveShown = true;
}

// What forces a redraw: everything but the clock (the bridge sends a
// heartbeat each minute; the clock is repainted with the next real change).
static bool differs(const Glance& a, const Glance& b) {
  return strcmp(a.state, b.state) || strcmp(a.task, b.task) || strcmp(a.next, b.next) ||
         strcmp(a.alert, b.alert) || strcmp(a.model, b.model);
}

void setup() {
  rgbLedWrite(RGB_BUILTIN, 0, 0, 120);
  Serial.begin(115200);
  SPI.begin(EPD_SCK, -1, EPD_MOSI, EPD_CS);
  display.init(115200, true, 2, false);
  memset(&cur, 0, sizeof cur);
  strcpy(cur.state, "idle");
  strcpy(cur.task, "waiting for the Mac");
  lastValid = millis();
  draw(cur, false);
  rgbLedWrite(RGB_BUILTIN, 0, 0, 0);
}

void loop() {
  while (Serial.available()) {
    char c = Serial.read();
    if (c == '\n') {
      line[len] = 0;
      Glance g;
      if (!overflow && len > 0 && parseLine(line, g)) {
        cur = g;
        lastValid = millis();
        if (noLink || differs(cur, shown)) { noLink = false; draw(cur, false); }
        rgbLedWrite(RGB_BUILTIN, 0, 20, 0); delay(30); rgbLedWrite(RGB_BUILTIN, 0, 0, 0);
      }
      len = 0; overflow = false;
    } else if (c != '\r') {
      if (len < RX_MAX) line[len++] = c; else overflow = true;
    }
  }
  if (!noLink && millis() - lastValid >= NOLINK_MS) {
    noLink = true;
    draw(cur, true);
  }
}
