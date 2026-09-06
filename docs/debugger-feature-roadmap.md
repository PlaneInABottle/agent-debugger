# Debugger Feature Roadmap — Uygulama Planı

> Dil: Türkçe (tanımlayıcılar/kod/komutlar İngilizce).
> Durum: **M-T frozen — Batch2 implementasyonuna hazır.** M0 bulguları donduruldu (debugpy 1.8.21 child akışı + Node 26.8.1 NodeWorker); M3/M4 Batch2'de implemente edilir, ardından tek FULL review. M5 M3/M4 sonrasını bekler.
> Kapsam: Başlangıçta istenen 5 yetenek (modül launch, Python child, Node worker, breakpoint kaldırma, eşzamanlılık) + sonradan eklenen 6. hedef-kimliği doğruluk milestone'u (M-I). Geniş refactoring yok. Bu plan dışında kod/test/runtime/process/bağımlılık/commit değişikliği yok.

1. Python `--module` / `python -m pytest` launch.
2. Python `subprocess` / `multiprocessing` child debugging.
3. Node `worker_threads` debugging.
4. Canlı breakpoint `remove` / `clear` (dört adaptörde).
5. Birden fazla outstanding debugger komutu (eşzamanlılık).

## Problem ve Başarı Kriterleri

- `py start` bugün yalnızca dosya yolu (`program`) kabul ediyor; `python -m pkg.mod` ve `python -m pytest …` kalıpları birinci sınıf vatandaş değil. Başarı: modül adı + modül argümanları ile launch, dosya launch ile aynı snapshot/breakpoint sözleşmesini verir (gerçek test-gövdesi breakpoint'i ile kanıtlanır, `--collect-only` ile değil).
- Python child process'ler ve Node worker'lar bugün tek hedefli (main-only) debug ediliyor; worker-only satırdaki breakpoint'ler sessizce timeout'a düşüyor. Başarı: child/worker hedefleri ortak hedef sözleşmesi (M-T) üzerinden görünür, adreslenebilir ve sınırlı kaynakla denetlenebilir — ya da deney + sınırlı fallback araştırması sonucu "desteklenmiyor" kararı açıkça belgelenir.
- Canlı oturumda yalnızca `breaks add` var (dört adaptörde; browser dahil); yanlış/kullanılmış breakpoint geri alınamıyor. Başarı: canlı `remove` ve kapsamı dürüst `clear`, saklı-kimlik eşleşmesi ve `stops.json` kalıcılığı ile dört adaptörde tutarlı çalışır.
- Tüm köprüler kesin sıralı komut işliyor; `continue`/`step`/`reload` arkasına düşen komut kuyrukta bekliyor. Başarı: canlı okumalar (`threads`/`breaks`/`logs`) bekleyen resume'u bloklamaz; ikinci rakip resume tanımsız kuyruk yerine tanımlı `busy` reddi alır; breakpoint mutasyonu ve `close` zamanlaması tanımlıdır; kullanıcı-cancel v1'de `close`'a ertelenmiştir (ayrı `cancel` komutu yok).
- Mevcut oturumlar/komutlar bozulmaz: eski `session.json`/`stops.json`, `status`/`context`/`close` sözleşmeleri korunur; `attach close` detach eder, asla yabancı hedefi öldürmez.

## Mevcut Durum Kanıtı

- `src/cli.rs:84-111` — `PyCmd::Start { program, python, stops, program_args }`; `--module` yok. `src/cli.rs:260-271` — `BreaksCmd::Add` yalnızca ekler; `Remove`/`Clear` yok.
- `src/spawn.rs:10-40` — `Target` enumunun gerçek tanım yeri burasıdır (`PyLaunch { program, python }` dosya-yolu varsayar; modül dalı yok). `src/spawn.rs:66-73` yalnızca `--program` üretir.
- `src/cli.rs:84-161` — attach girdileri yalnızca `host`/`port` (+ browser `tab`); **pid girdisi yoktur** (java/py/node).
- `bridge/py/src/pybridge.py:926-941` — `handshake_attach` doğrudan `--listen` sunucusuna DAP bağlanır (`initialize`→`attach`→`configurationDone`); process kimliği toplamaz. `bridge/node/src/nodebridge.js:579-607` — `discoverAttach` `/json/list`'ten ilk node hedefini seçer ama girdiyi atar, yalnızca `webSocketDebuggerUrl` döner. `bridge/browser/src/browserbridge.js:398-400,563-565` — browser tam `/json/list` girdisini saklar (`tabJson {id,title,url}`); process cwd/argv yolu yoktur.
- `src/session.rs:355-454` — `spawn` bekleme döngüsü: `session.json` (köprü yazar) + ilk `context`/`threads` doğrulaması; başarısız kurulum dizini wholesale siler (sızıntısız retry). `src/session.rs:569-632` — `status`/`session_entry` `session.json` (köprü-bakımlı) + `stops.json` (CLI-bakımlı `target`: istenen bayrakların özeti, bağımsız gözlem DEĞİL) okur.
- `src/main.rs:61-92` — `py/node start|attach` → `spawn::cmd_spawn` bağlantısı; `src/main.rs:183-191` — `Breaks { cmd }` → çıplak `breaks` `session::forward`, `Add` → `session::cmd_breaks_add` bağlantısı (kaldırma bağlantısı yok; M2 aynı deseni izler).
- `bridge/py/src/pybridge.py:430-504` — `parse_args` yalnızca `--program`; `--module` `Usage` üretir. `bridge/py/src/pybridge.py:882-898` — `handshake_launch` DAP `launch`'a yalnızca `{"program", "args", "justMyCode": True, "console": "internalConsole"}` gönderir.
- `bridge/py/src/pybridge.py:863-880` — tek `debugpy.adapter` child; tek `DapConn` (`bridge/py/src/pybridge.py:125-220`). `bridge/py/src/pybridge.py:978-1058` — `arm_breakpoints` tek hedef varsayar. `bridge/py/src/pybridge.py:1481-1634` — `cmd_breaks_add` yalnızca ekler. `bridge/py/src/pybridge.py:1677-1707` — `dispatch` tek thread. `bridge/py/src/pybridge.py:1860-1901` — `serve` katı sıralı accept/dispatch.
- `bridge/node/src/nodebridge.js:25-28` — main-only sınırı. `bridge/node/src/nodebridge.js:514-520` — `startTarget` yalnızca `--inspect-brk` + `program`. `bridge/node/src/nodebridge.js:642-664` — handshake'te `Target.*`/`NodeWorker.*` yok. `bridge/node/src/nodebridge.js:831-839` — tek `paused` slotu. `bridge/node/src/nodebridge.js:1417-1533` — `cmdBreaksAdd` yalnızca ekler. `bridge/node/src/nodebridge.js:1555-1571` — `dispatch` sıralı. `bridge/node/src/nodebridge.js:1601-1652` — `serve` kuyruklu ama sıralı.
- Dördüncü adaptör (browser): `bridge/browser/src/browserbridge.js:82-107` — `parseBreak` dosya yolu değil **URL fragmanı** (`frag:line`) saklar (realpath yok). `bridge/browser/src/browserbridge.js:177-179` — `fragRegex` (`(^|/)frag([?#]|$)`). `bridge/browser/src/browserbridge.js:200-` — `dedupeStartupBreaks` frag-anahtarlı. `bridge/browser/src/browserbridge.js:448-530` — `armBreakpoints` (gölge kuralı dahil). `bridge/browser/src/browserbridge.js:1299-1402` — `cmdBreaksAdd` yalnızca ekler. `bridge/browser/src/browserbridge.js:1404-1421` — `dispatch` (dahil `reload` → `cmdReload`). `bridge/browser/src/browserbridge.js:1208-1233` — `cmdReload` (parkı düşürür, `Page.reload`, sonra `pump`). `bridge/browser/src/browserbridge.js:1443-1494` — `serve` nodebridge ile aynı sıralı kuyruk şekli. `bridge/browser/src/browserbridge.js:1518-1566` — attach-only, startup `pump` yok, `attach --break` yalnızca ARM eder. `bridge/browser/src/browserbridge.js:1423-1432` — `cleanup` yalnızca detach (`Debugger.disable` + kapatma; sekme yaşamaya devam eder). `bridge/browser/src/browserbridge.js:548-561` — `verifyTab` (ölü sekmede `breaks` dahil komutlar yalan söylemez).
- Kaldırma ilkelleri (doğruluk notu): DAP `setBreakpoints` dosya-başına replace eder (`bridge/py/src/pybridge.py:978-980` yorumu). JDI `deleteEventRequest` repo içinde yalnızca step temizliğinde kullanılır (`bridge/java/src/BridgeSession.java:672`); breakpoint üretimi `bridge/java/src/BridgeConn.java:221`. **Repo genelinde `removeBreakpoint` kullanımı yoktur** (doğrulanmış sıfır eşleşme): CDP `Debugger.removeBreakpoint` protokol yeteneğidir, yerel doğrulanmış ilkel değildir — M2 onu kullanır ve canlı testle doğrular.
- Java gölge mekaniği: `bridge/java/src/BridgeSession.java:761-768` — `isShadowed`/`shadowDetail`; `bridge/java/src/BridgeSession.java:157-158,196-198` — gölgelenmiş logpoint'e JDI isteği kurulmaz; `bridge/java/src/BridgeEval.java:150-153` — gölgelenmiş logpoint asla ateşlenmez (satır break'indir). Sonuç: gölgeli logpoint'in arkada bekleyen bir plant'i yoktur; "otomatik dirilme" tanımsızdır, M2 açık yeniden-kurma kuralı koyar.
- `bridge/java/src/BridgeSession.java:540-677` — `dispatch` (`breaksAdd` var, kaldırma yok). `bridge/java/src/BridgeSession.java:806-887` — `breaksAddJson` (iki fazlı validate-then-arm). `bridge/java/src/BridgeSession.java:460-505` — sıralı `serve`.
- `src/session.rs:87-118` — `cmd_breaks_add` (confirmed-only kalıcılık; transport hatasında kalıcılık atlanır + `bare breaks` uyarısı). `src/session.rs:153-166` — `forward` tek istek/tek yanıt. `src/session.rs:520-563` — `close` (65 sn kuyruk toleransı + 15 sn port-ölüm kontrolü).
- `src/client.rs:16-48` — bağlantı başına tek framed istek: **yanıt zaten bağlantıyla koreledir**; multiplex/istek-kimliği yoktur ve v1'de eklenmez (`MAX_FRAME_BYTES` 64 MiB). `src/dap.rs:1-88` — taşıma; oturum kimliği tanımı yok.
- `skills/agent-debugger/SKILL.md:76-83` — yalnızca canlı line `add`; resume beklerken ekleme kuyruk arkası. `SKILL.md:302-304` — worker main-only. `SKILL.md:254-265,287-293` — CWD-kazanır sonra `--src` resolver. `SKILL.md:355-356` — launch `close` kendi hedefini öldürür, attach detach eder.
- Fixture'lar: `examples/node-demo/worker.js:1-8` (main+worker), `examples/py-demo/threads.py:1-27` (thread; process değil).
- Test yüzeyleri: `cargo test` (27 unit), `python3 tests/test_pybridge.py` (unittest), `node --test tests/*.test.js` (bridge'i `main()` kuyruğu stripped yükler), `tests/test_live.py` (`test_04` browser+reload, `test_07/08` canlı `breaks add` incl. browser, `test_22` browser step; `target/debug/agent-debugger` + izole `HOME` ister).

## Yaklaşım

En küçük tasarım: tek-binary + localhost `Content-Length` JSON + dil başına daemon korunur; yeni yetenekler geriye-uyumlu sözleşme ekleridir. Sıralı repo düzeni (çakışan dosya paylaşımları yüzünden paralel implementasyon yok):

1. **M0 Deney** — yalnızca multi-target (M3/M4) için önkoşul. M1/M-I/M2 M0'dan bağımsızdır. Prob çalışması repo'ya yazmaz (onaylı geçici dizin), bu yüzden takvim olarak M1/M-I/M2 ile çakışabilir; repo düzenlemeleri kesin sıralıdır: M1 → M-I → M2 → M-T → M3 → M4 → M5.
2. **M1 Modül launch**, sonra **M-I Hedef-kimliği**, sonra **M2 Kaldırma** (sıralı: üçü de `src/cli.rs`, `src/session.rs`, `src/main.rs` dispatch, testler ve SKILL'e dokunur; `src/session.rs` bölünmez).
3. **M-T Ortak hedef sözleşmesi** (M0 bulgusu + M2 kimliği üzerine; kod yok/hafif) → **M3 Python**, sonra **M4 Node** (ikisi de ortak sözleşmeyi implement eder; bağımsız tasarım yok; paylaşılan CLI/session/SKILL/test mutasyon çakışmalarını önlemek için bilinçli sıralı implementasyon).
4. **M5 Eşzamanlılık** en son (dört köprünün `serve`/`dispatch` yolu; gate M-I + M2 + dondurulmuş M-T metnidir; multi-target maddeleri GO olan kola koşulludur, iki kol da kesin NO-GO ise M5 tek-hedef eşzamanlılıkla ilerler).

KISS/YAGNI: `pybridge.py`, `src/session.rs`, `BridgeSession.java`, `nodebridge.js`, `browserbridge.js` yalnızca gerektiği kadar büyür, bölünmez. Gözlenen sürümler (`node v26.8.1`, `python3 3.14.6`) destektir-gözlemidir, garanti taban değildir; destek matrisini M0 dondurur. Tek mekanizmanın desteklenmemesi doğrudan NO-GO değildir: önce sınırlı fallback araştırması (diğer mekanizma / belgeli launch bayrağı); bayrak gereksinimi tek başına NO-GO sayılmaz (sessiz enjeksiyon yasaktır, belgeli opt-in kabul). Bayrak adları operasyoneldir, milestone-içidir; maddi kullanıcı kararı değildir.

## Doğrulanmış Gerçekler / Varsayımlar

| Öğe | Kanıt | Durum |
|-----|-------|-------|
| DAP `launch` `module` alanını destekler | Context7 `/microsoft/debugpy` "Launch Configuration with Module"; Wiki `Debug-configuration-settings` | DOĞRULANDI (doküman) |
| `python -m debugpy --listen … -m pytest …` kalıbı | Context7 "CLI: Debug a Module"; `README.md` | DOĞRULANDI (doküman) |
| `subProcess` launch alanıdır; attach için `debugpy.configure` / `--configure-subProcess` | Wiki `Command-Line-Reference`, `API-Reference`, VS Code docs | DOĞRULANDI (doküman) |
| Child akışı çok-hedeflidir (`debugpyAttach`/`ptvsd_subprocess` + `processId` ile ayrı attach) | `doc/Subprocess debugging.md` | DOĞRULANDI (doküman; güncel şema M0'da) |
| Node worker için iki aile: `NodeWorker.*` ve `Target.*` flat | `vscode-js-debug`, CDP `Target` dokümanı | DOĞRULANDI (doküman; kurulu sürüm davranışı M0'da) |
| `--experimental-worker-inspection` geçmişte gerekti | `nodejs/node#56759`, `#56343` | DOĞRULANDI (doküman; güncel gereklilik M0'da) |
| Browser kimliği URL fragmanıdır, realpath değildir | `browserbridge.js:82-107,177-179` | DOĞRULANDI (repo) |
| Browser attach-only + startup pumpsuz + detach-only close + `verifyTab` | `browserbridge.js:1518-1566,1423-1432,548-561` | DOĞRULANDI (repo) |
| Dört `serve` de sıralıdır; yanıt bağlantıyla koreledir | py/node/browser `serve`, `BridgeSession.java:460-505`, `client.rs:16-48` | DOĞRULANDI (repo) |
| `removeBreakpoint` repo içinde kullanılmıyor | sıfır eşleşme (doğrulanmış grep) | DOĞRULANDI (repo; CDP yeteneği, yerel ilkel değil) |
| Java gölgeli logpoint'in bekleyen plant'i yoktur | `BridgeSession.java:157-158,196-198,761-768`; `BridgeEval.java:150-153` | DOĞRULANDI (repo) |
| `Target` enumu `src/spawn.rs:10-40`'tadır; `main.rs:183-191` breaks bağlantısıdır | repo | DOĞRULANDI (repo) |
| debugpy child olay adı/payload/çoklu-bağlantı şeması | M0 probu (debugpy 1.8.21): `debugpyAttach` event + child-başına DAP oturumu (attach = olay gövdesi verbatim, yanıt `configurationDone` sonrası) | DOĞRULANDI (M0; yalnızca M3 için) |
| Kurulu Node'da flagsiz discovery + worker yönlendirme | M0 probu (v26.8.1, bayraksız): `Target.*` flat YOK (NO-GO); `NodeWorker` sarmalı GO (flat `sessionId` yok, wrap gerekir) | DOĞRULANDI (M0; yalnızca M4 için) |

Referanslar: `https://github.com/microsoft/debugpy/wiki/Debug-configuration-settings`, `.../Command-Line-Reference`, `https://github.com/microsoft/debugpy/blob/main/doc/Subprocess%20debugging.md`, `https://code.visualstudio.com/docs/python/debugging`, `https://chromedevtools.github.io/devtools-protocol/tot/Target/`, `https://github.com/nodejs/node/pull/56759`, `https://github.com/microsoft/vscode-js-debug/blob/main/src/targets/node/nodeLauncherBase.ts`.

## Kapsam

İzinli: `src/cli.rs`, `src/spawn.rs` (`Target`), `src/main.rs` (dispatch bağlantısı), `src/session.rs`, `src/client.rs`, `src/dap.rs` üzerinde geriye-uyumlu ekler; `bridge/py/src/pybridge.py`, `bridge/node/src/nodebridge.js`, `bridge/browser/src/browserbridge.js`, `bridge/js/cdp_conn.js` (yalnızca M0 gerektirirse flat `sessionId` taşıma), `bridge/java/src/BridgeSession.java` (+ gerekirse `BridgeProto.java` dizi parse); `docs/debugger-feature-roadmap.md` + implementasyon milestone'larında `skills/agent-debugger/SKILL.md` notları.

Yasak: yukarıdaki dosyaları sırf boyut için bölmek; `attach`'in mevcut child/worker keşfi vaadi; `session.json`/`stops.json`/`status` bozucu değişiklik; `close` sahipliğini sessizce değiştirmek (attach detach kalır); multiplex/istek-kimliği mimarisi (v1 dışı); sessiz hedef-process bayrak enjeksiyonu; onaylı geçici dizin dışında throwaway prob artığı; bu plan dışında implementasyon/test/runtime/process/bağımlılık/commit.

## Blast Radius ve Değişmezler

- Giriş noktaları: `py/node start|attach`, `browser attach`, `continue|step|reload|context|eval|vars|stack|threads|breaks|logs|close|status|doctor`.
- Değişmez 1 — Taşıma: `MAX_FRAME_BYTES` 64 MiB + 8192 B header; bağlantı başına tek istek/tek yanıt korelasyonu.
- Değişmez 2 — Park kararlılığı (hedef-bazlı; M-T'de bir hedefin stop'u diğerini oynatmaz).
- Değişmez 3 — Çalışan hedef eski kareleri GÜNCEL stop gibi sunmaz: kare-bağlı okumalar (`context`/`vars`/`stack`/`eval`/`step`) parksızken fail-fast (`no stopped thread`); yalnızca canlı okumalar (`threads`/`breaks`/`logs`) parksız cevaplanır.
- Değişmez 4 — `close` tür-bağımlıdır: launch kendi başlattığını biçer, attach (dört adaptörde, browser dahil) yalnızca detach eder; yabancı/bağsız hedef asla öldürülmez.
- Değişmez 5 — Kalıcılık: yalnızca confirmed mutasyon `stops.json`'a yansır (atomik tmp+rename); transport hatasında kalıcılık atlanır + `bare breaks` ile uzlaşma.
- Değişmez 6 — M5'e kadar sıralı serve; "resume beklerken mutasyon yapma" kuralı geçerlidir.

## Bağımlılık Grafiği

```text
M0 Deney ──→ (yalnızca multi-target için önkoşul) ──→ M-T sözleşme (GO olan kol kapsamına açılır) ──→ M3 → M4 → M5
M1 Modül ──→ M-I Kimlik ──→ M2 Kaldırma (sıralı, paylaşılan dosyalar) ──→ M-T (kaldırma kimliği gerekir)
M1, M-I, M2 M0'dan bağımsızdır. M0 probu repo'ya yazmaz; repo düzenlemeleri kesin sıralıdır.
M-T sonrası M3/M4 ortak sözleşmeyi implement eder (paralel repo düzenlemesi yok).
Her NO-GO kolu kapanır; iki kol da kesin NO-GO ise M-T hattı kapanır, M5 tek-hedefle ilerler.
```

## Milestone 0: Fizibilite Deneyi — Child/Worker Probları (yalnızca M3/M4 blokeri)

- Bağımlılık: Yok. M1/M2'yi bloklamaz.
- Alt-fazlar:
  1. **Python probu:** `subProcess:true` launch'ta olay adı/payload (`processId`?), child başına ek DAP bağlantısı gerekip gerekmediği, `spawn`/`fork` + `subprocess.Popen` farkı, pytest-altı davranış, sonlandırma yayılımı (launch disconnect), sınır davranışı.
  2. **Node probu:** gözlenen sürümde flagsiz `Target.setAutoAttach {flatten:true}` ve `NodeWorker.enable` yanıt/olayları, `sessionId` yönlendirme, `--inspect-brk` kalıtımı, worker exit bildirimi; desteklenmeyen mekanizmada sınırlı fallback (diğer mekanizma; belgeli launch bayrağı).
- Dosya/davranış sınırı: **Repo değişikliği yok.** Problar onaylı geçici dizinde (`/var/folders/c8/nkbhh7z16llc1lyzgz3hv7xr0000gp/T/opencode`); `agent-debugger` implementasyonu yok; bulgular bu plana işlenir, commitlenmez.
- INNER/CHECKPOINT: `python3 -c "import debugpy…"`, `node --version`, `doctor` çıktısı, ham DAP/CDP log özeti (secret yok).
- Kabul: her mekanizma için destek-matris satırı (olay, payload şeması, plant kanıtı, exit sahipliği, attach-keşif beklentisi HAYIR) + fallback kararı. Tek mekanizma yokluğu = o mekanizma için NO-GO + fallback'a geç; tüm bounded fallback'ler tükenmeden özellik NO-GO ilan edilmez; bayrak gereksinimi tek başına NO-GO değildir.
- Kanıt geçersizleşmesi: `debugpy`/`node` sürümü değişirse prob tekrarlanır.
- Milestone gate: GO/fallback/NO-GO satırları + bu planın M-T öncesi güncellemesi + review. M-T/M3/M4 sözleşmeleri M0 dondurmadan implement-ready sayılmaz.
- Rollback: gereksiz (değişiklik yok); geçici dizin temizliği.

### M0 Bulguları (donduruldu; M0-review bekler)

Ortam (pin): macOS arm64; `python3 --version` CPython 3.14.6; provisioned venv yorumlayıcısı `debugpy.__version__` 1.8.21; `node --version` v26.8.1. Problar repo-dışı ham protokoldür (stdlib DAP istemcisi; CDP için provisioned `ws` salt-okunur require): ürün köprüsü kullanılmadı, repo'ya yazılmadı, secret/PID persist edilmedi.

**Python (debugpy 1.8.21) — GO (üç dal):**
- Olay: `debugpyAttach` (event). Gövde anahtarları (sırasız): `program, justMyCode, console, subProcess, python, isOutputRedirected, name ("Subprocess <pid>"), subProcessId (<pid>), connect {host 127.0.0.1, port <internal>}, request ("attach")`. `supportsStartDebuggingRequest` bildirilmediğinde event yolu gelir (M3 bu yolu kullanır).
- Yönlendirme: AYNI adaptör portuna ikinci eşzamanlı DAP bağlantısı + `initialize` → `attach` (olay gövdesi verbatim argüman) → `setBreakpoints` → `configurationDone`; `attach` yanıtı `configurationDone` SONRASI gelir (launch ile aynı pipeline; önce yanıt beklemek deadlock'tur). Çıplak `{"subProcessId"}` attach YANIT ÜRETMEZ (`debugpyWaitingForServer` sonrası `terminated`) — gövde şarttır.
- Plant/hit: `setBreakpoints verified:[True]`; `stopped reason:breakpoint` + `stackTrace` hedef dosya/satırda (Popen: `<module>` marker satırı; spawn: `run` marker satırı; fork: `<module>` marker satırı, `threadId:2`).
- Dallar: `subprocess.Popen` GO; `multiprocessing` spawn GO — ancak 1 `Process` başına **2 child oturumu** gözlendi (worker + yardımcı; yalnızca worker kullanıcı kodunu koşturur; child `process {name:'-c', startMethod:'attach'}`) → `maxTargets`/ignored sayacı bunu hesaba katar; `os.fork` GO (bu stack'te ayrı `debugpyAttach` + yönlendirilebilir oturum üretir).
- Sahiplik: child-oturum `disconnect` child'ı ÖLDÜRMEZ (yaşamaya devam); parent `disconnect {terminateDebuggee:true}` tüm ağacı biçer (child dahil); sonunda artık süreç yok (`pgrep` temiz).
- Varsayılan: `subProcess` anahtarı YOKKEN de child olayı+oturum+hit çalıştı (1.8.21'de izleme default-ON). M-T'deki `--subprocess` opt-in'i debugpy'yi "açmaz"; köprünün olayı işleme/yönlendirme anahtarıdır; bastırmak için launch'a açık `subProcess:false` gerekir (kendi launch isteğimizin alanı; hedefe sessiz enjeksiyon değildir).
- Attach keşfi (sınırlı): `--listen` hedefe `attach` eden oturumda da Popen-child için `debugpyAttach` OLAYI gözlendi; attach-üzeri child-oturum yönlendirme denenmedi (kapsam dışı) → attach tek-hedef kalır.

**Node (v26.8.1, bayraksız) — GO (NodeWorker; Target-flat NO-GO):**
- `Target.setAutoAttach`/`Target.getTargets` hedef-kapsamlı oturumda YOK (`'Target.setAutoAttach' wasn't found`): flat `sessionId` mekanizması bu stack'te NO-GO.
- Seçilen mekanizma: `NodeWorker.enable {waitForDebuggerOnStart:true}` (bayraksız OK; main resume ÖNCESİ enable edilir) → `NodeWorker.attachedToWorker {workerInfo.type:'worker', url}`. Yönlendirme FLAT DEĞİLDİR: `NodeWorker.sendMessageToWorker {sessionId, message:<stringified-CDP>}` + `NodeWorker.receivedMessageFromWorker` sarmalı gerekir; çıplak `sessionId` alanlı `Debugger.*` çağrısı yanlış oturuma gider.
- Plant/hit: `Debugger.setBreakpointByUrl {lineNumber:<0-tabanlı>, url:file://…}` → `breakpointResolved {lineNumber}`; döngüde her iterasyonda `Debugger.paused` — `reason:'other'` ETİKETİNE RAĞMEN `hitBreakpoints:[<plant-ID>]` (plant ID ile birebir aynı) → hit hükmü `hitBreakpoints` ile verilir, `reason` ile DEĞİL (M4 kuralı). URL eşleşmesi `/var`→`/private/var` normalizasyonuna toleranslıdır (M4 resolver notu).
- Exit: worker doğal çıkışında (exit 0; main'deki `w.on('exit')` ile de gözlendi) `NodeWorker.detachedFromWorker` gelir. Launch kill ağacı biçer; sonunda artık süreç yok.
- Attach keşfi (sınırlı): `/json/list` yalnızca main `node` hedefini listeler (worker girdisi YOK); `/json/version`'da browser WS YOK → attach-üzeri worker keşfi desteklenmiyor beyanı için yeterli; genişletilmedi. `--experimental-worker-inspection` gerekmedi (bayraksız GO; bayraklı varyant koşulmadı).

**UNVERIFIED:** pytest-altı child davranışı (pytest venv'de kurulu değil; kurulum yasak) → M3 pytest'i açıkça kapsamaz; ayrı prob gerekir.

**M-T'ye dondurulanlar (işlendi):** Python = `debugpyAttach` + child-başına DAP oturumu (gövde-verbatim attach); Node = `NodeWorker` sarmalı (flat yok, fallback yok); hedef kimlikleri `child:<pid>` / `worker:<sessionId>`; **8 aktif non-main + 16 exited geçmişi** + helper/bırakma kuralları M-T metninde frozen; attach tek-hedef kalır; Node hit-atfı `hitBreakpoints` iledir.

## Milestone 1: Python `--module` / `pytest` Launch (M0'dan bağımsız)

- Bağımlılık: Yok. (M3'ün önkoşulu; M2 ile sıralı: önce M1.)
- Alt-fazlar:
  1. CLI+spawn+özet: `py start` için dosya `program` ile karşılıklı dışlayıcı modül formu: `Target::PyLaunch` tam-olarak-biri (`exactly-one-of`) `program`/`module` taşır, clap çakışmayı hedef çalışmadan fail-fast reddeder; dosya launch yolu değişmez. `target_summary` + `stops.json` hedefine literal `{"module": M}` anahtarı (`Target` tanımı `src/spawn.rs:10-40`'ta genişler; bağlantı `src/main.rs` desenini izler). Modül adı hedef çalışmadan noktalı-tanımlayıcı doğrulamasından geçer (her segment `[A-Za-z_][A-Za-z0-9_]*`, nokta-ayrımlı; alt-çizgi/sayı segment-içinde serbest).
  2. Köprü: `pybridge.py Config`/`parse_args`/`handshake_launch` DAP `launch`'ta `module` alanı; breakpoint resolver değişmez (modül çözümlemesi debugpy'nindir; dosya-varlık kontrolü modüle uygulanmaz).
- Sözleşme (proposed):
  ```bash
  agent-debugger --session pymod py start --module mypkg.mod -- --arg 1
  agent-debugger --session pytest py start --module pytest -- tests/test_x.py -q
  ```
  ```json
  {"request": "launch", "module": "pytest", "args": ["tests/test_x.py", "-q"], "justMyCode": true, "console": "internalConsole"}
  ```
- Varsayılan değişmez (dosya launch); çift form fail-fast. Geriye uyumluluk: dosya launch DAP isteği bayt-aynı; eski `stops.json` null-tolerant okunur.
- Negatif/yaşam-döngüsü: olmayan modül → `error.json` + ad yeniden kullanılabilir; `timeout`ta hit yoksa launch fail (attach fallback'u yok); launch `close` biçer.
- Doğrulama (repo-native; bu turda test koşulmuyor — plan yalnızca):
  `cargo test`, `cargo build`, `cargo fmt --check`, `python3 tests/test_pybridge.py`,
  canlı: gerçek test-gövdesi satırında breakpoint (collect-only kabul değil) + argüman geçişi + cwd göreli import + yorumlayıcı bayrağı (`--python`) + bağımlılık-negatifi (olmayan modül fail-fast) + `status`/`close confirmed`.
  Mevcut süitler regresyon içindir; modül davranışı için **yeni** unit (CLI/spawn parse + `target_summary`) ve canlı testler şarttır.
- Gate: yukarıdaki matris PASS + `context`/`breaks` plant dosya-launch ile eşdeğer.
- Stop: debugpy `module` reddederse STOP. Rollback: bayrak/alan silinir.

## Milestone M-I: Attach Hedef-Kimliği (M1 sonrası, M2 öncesi; M0'dan bağımsız)

Kullanıcı niyeti: attach hangi program/process'i seçtiğini göstermiyor; aynı kaynak yolu yanlış process'te verify olup hiç vurmayabiliyor ve bu, ulaşılamaz kod gibi görünüyor. Attach yanıtı hedef komut satırı + cwd taşımalı ki yanlış-process seçimi hemen belli olsun.

- Bağımlılık: M1 sonrası (paylaşılan `src/cli.rs`, `src/session.rs` status, `src/main.rs`, testler, SKILL). M0'dan bağımsız. M2 bu milestone'dan sonra gelir.
- Alt-fazlar:
  1. **Sınırlı mekanizma keşfi (prob; repo değişikliği yok; onaylı geçici dizin):** her adaptörde bağımsız gözlem kaynağı var mı? (a) host:port attach'te protokolün bildirdiği PID/metadata (DAP/CDP/JDWP yanıt-alanları; ham logla doğrula); (b) PID bilindiğinde Rust-tarafı OS-yerel exe/argv/cwd okuma imkânı (macOS mevcut ortam + Linux CI/fixture varsa; Windows desteklenmiyor) — yedek hiyerarşi önden reçete edilmez (`lsof`/`proc`/`ps` dayatılmaz), deney dondurur; (c) browser: mevcut `/json/list` girdisi + bağlı CDP oturum metadata'sı yeterlidir (`tabJson` emsali `browserbridge.js:563-565`), process iddiası yok. Hedef-kodda eval/import ile metadata toplama YOKTUR (ayrı güvenlik review'suz yasak). Stop: bir adaptörde bağımsız kaynak hiç bulunamazsa o adaptörde `observedTarget` daimi `unavailable` + gerekçeli olur; sözleşme yine donar (kapsam kararı, başarısızlık değil).
  2. **Sözleşme implementasyonu:** aşağıdaki public şema + redaksiyon + kalıcılık + tanı-ipucu.
- Public şema (proposed, additive; eski alanlar değişmez):
  ```json
  "requestedTarget": {"host": "localhost", "port": 5678, "tab": null, "pid": null},
  "observedTarget": {"kind": "process", "pid": 12345, "executable": "/usr/bin/python3",
    "argv": ["python3", "worker.py"], "cwd": "/repo/svc",
    "source": "<prob-dondurur>", "observedAt": 1735689600,
    "unavailable": [], "warnings": []}
  ```
  Browser (process iddiası YOK):
  ```json
  "observedTarget": {"kind": "tab", "url": "http://h/app.js", "title": "Shop",
    "targetId": "ABC", "debugEndpoint": "localhost:9222",
    "cwd": null, "argv": null, "notApplicable": ["cwd", "argv"],
    "source": "cdp-target-list", "observedAt": 1735689600,
    "unavailable": [], "warnings": []}
  ```
  Metadata-yok örneği: `"observedTarget": {"kind": "process", "pid": null, "executable": null, "argv": null, "cwd": null, "source": null, "observedAt": …, "unavailable": [{"field": "cwd", "reason": "no-independent-source"}], "warnings": ["identity-unverified: showing attach endpoint only"]}`.
  - İstenen PID asla gözlem diye sunulmaz: `requestedTarget.pid` yalnızca CLI girdisi taşıdığında dolar (v1 CLI'da pid girdisi YOK → null, rezerve alan); `observedTarget.pid` yalnızca bağımsız kaynaktan gelir. Caps: alan-başına ≤512 chars, toplam `observedTarget` ≤2KB; env ASLA toplanmaz.
- Gizlilik/redaksiyon (non-negotiable): `--token <v>`, `--password=<v>`, `authorization`/`api-key`/`apikey`/`secret`/`passwd`/`pwd` varyantları (`=`/`:`/boşluk ayraçlı, case-insensitive) → `[redacted]`; insan + JSON çıktısı aynı formu kullanır. Kısaltma göstergesi ortak sözleşmedir: `… (+N more chars)` — dördünde de mevcut deyimdir (py `trunc_str` `pybridge.py:509-512`, node `truncStr` `nodebridge.js:394-396`, browser `truncStr` `browserbridge.js:166-168`, Java `BridgeSnapshot.java:233`); implementasyonlar mevcut yardımcıyı yeniden kullanır/genişletir, yeni deyim icat edilmez. Ham komut satırı ASLA persist edilmez (`session.json`/`stops.json`/`error.json`/`logs`). Öneri KABUL: redakte `observedTarget` `session.json`'da persist edilir (atomik; sahibi köprüdür — dördü de kendi yazar: py `publish_state` `pybridge.py:837` via `write_file` `:1745` → `:849`; node `publishState` `nodebridge.js:1060` via `writeFile` `:433` → `:1066`; browser `publishState` `browserbridge.js:887` via `writeFile` `:318` → `:893`; Java `BridgeSession.publishState` `:690` via `BridgeProto.writeFile` `BridgeProto.java:21` → `:700`) → `status` okur, `context`/attach-yanıtı önbellekten yansıtır (komut-başına yeniden prob YOK). Tazelik: handshake'te bir kez yazılır, oturum ömrünce immutable; exit sonrası last-known kalır.
- Yaşam-döngüsü: metadata yokluğu attach'i BAŞARISIZ kılmaz. Uyumsuzluk (istenen pid mevcut VE gözlenen farklı) → arm ÖNCESİ doğruluk hatası, temiz detach, oturum sızıntısı yok (`src/session.rs:390-398` wholesale-temizlik emsali). Metadata uyarısı `close` sahipliğini değiştirmez (launch biçer / attach detach).
- Yüzeyleme: attach immediate yanıt + `status` satırı + `context` aynı redakte önbelleği taşır. Timeout/vurulmayan-breakpoint çıktısına kompakt kimlik ipucu eklenir (kök-neden iddiası YOK). `breaks verified` = adaptör kabulü/plant demektir, çalıştırma kanıtı DEĞİL.
- Launch tutarlılığı: launch aynı şekli kullanır (`requestedTarget` CLI bayraklarından, `observedTarget` köprü-gözleminden; bağımsız gözlem yoksa unavailable + gerekçe; requested asla observed sayılmaz). Process envanteri/seçim UI YOK (kapsam dışı).
- Doğrulama (bu turda koşulmuyor): unit redaksiyon/truncation/no-env + no-secret taraması; PID pozitif/yok/uyumsuz; launch tutarlılığı; aynı-dosya-yolu yanlış-process canlı fixture'ı; browser URL/title; üç dilde attach lifecycle temizliği + `close confirmed`; mevcut full gate'ler. Ortam: macOS (mevcut) + Linux (CI/fixture varsa); diğerleri açıkça desteklenmiyor.
- Gate/stop/rollback: redaksiyon sızıntısı veya ham-secret persist bulgusunda STOP; rollback ek-alanları kaldırır.
- Kullanıcı kararı: YOK önerilir (güvenlik varsayılanları non-negotiable).

## Milestone 2: Canlı Breakpoint `remove` / `clear` (dört adaptör)

- Bağımlılık: M-I sonrası (paylaşılan `src/cli.rs`, `src/main.rs`, `src/session.rs`, testler, SKILL). M0'dan bağımsız.
- Alt-fazlar:
  1. CLI+protokol+kalıcılık: `breaks remove --break SPEC…` ve yalnız-line-break `breaks clear` (**v1'de bayraksız; yalnızca line breakpoint'leri düşürür, kapsam help metninde açık yazılır; fazla argüman clap ile reddedilir; `--kind` spekülatiftir, v1 dışı**). `src/main.rs:183-191` deseninde dispatch bağlantısı; `src/session.rs`'te confirmed-eksiltme (`append` aynası, atomik tmp+rename).
  2. Dört köprüde kaldırma: py (`setBreakpoints` merge-without-removed), node+browser (`Debugger.removeBreakpoint` + `breakIdToRec` silme), Java (`deleteEventRequest` + cfg listelerinden düşürme).
- Kimlik (adaptör-başına, saklı-kayıt eşleşmesi — add resolver'ın varlık/satır kontrolleri YOK):
  - Kaldırma girdisi, add ile aynı sözlüksel/kanonik oluşumla normalize edilir (Python/Node: realpath oluşumu; Java: `cls:line`; browser: `frag:line`; `|cond` ayrımı dahil) ancak dosya-varlık/satır-aralık kontrolleri uygulanmaz: kaynak silinmiş/değişmiş olsa da kaldırma çalışır. realpath sözlüksel çalışıp yazım farklı olabileceğinden eşleşme saklı ham-kimlik (stored raw) yedeğiyle yapılır.
  - Köprü `removed[]` girdilerinde ilk başta kalıcılaşmış/saklı ham kimliği yansıtır (kaldırma girdisinin yazımı değil); session, yalnızca confirmed kalıcı ham-kayıtları (`persisted raws`) `stops.json`'dan çıkarır. Ham-tekrar/koşul çakışmasında (`path:line` yalın yalnızca koşulsuzu, koşullu tam `|cond` ister) eşleşmeyen `missing` sayılır (`ok:false` değil).
- Backend batch işlemi: önce tamamı doğrula/eşleştir (safha 1, sıfır backend mutasyonu), sonra öğe-başına backend çağrısı (safha 2); başarılılar confirmed-removed, başarısızlar korunur + `failed[{raw,spec,error}]` (+ `warning`); toplam backend arızası `ok:false`, mutasyon yok; kalıcılık yalnızca confirmed başarılara uygulanır. DAP dosya-başına replace, backend yanıtı granülerliğinde grup-atomiktir (dosya yanıtı OK ise o dosyanın confirmed'ları düşer).
- Java gölge kuralı (doğru ve çalıştırılabilir): break kalkınca gölgeli logpoint otomatik dirilmez (ortada plant yoktur). Tanımlı davranış: aynı-satır gölgeli logpoint için köprü mevcut logpoint-kurma yolunu bir kez dener — sınıf yüklüyse kurar (kayıt `armed`/`verified`/`pending` + gerçek detay), sınıf yüklü değilse mevcut deferred-watch yoluna bırakır (`pending`, "class not loaded yet (deferred)"); kurma başarısızsa kayıt `pending` + hata detayı + `warning` olur. (Node/browser: mevcut `setBreakpointByUrl` logpoint dalı yeniden çalışır; Python: kalanların merge'i logpoint'i doğal taşır.)
- Kısmi backend arızası + uzlaşma/kalıcılık: yukarıdaki batch kuralı; transport timeout'unda kalıcılık atlanır + `bare breaks` uzlaşma mesajı (`cmd_breaks_add` hata kalıbı). `stops.json` diğer alanları değer-korumalı (parsed-değer eşitliği; testler parsed karşılaştırır, bayt karşılaştırmaz).
- Reload (browser): plant `cfg`'den yeniden kurulur; kaldırılmış geri gelmez. `verifyTab` ölü sekmede kaldırmayı reddeder.
- Negatifler: boş liste fail-fast; exit etmiş hedef `close this session`; resume beklerken mutasyon M5'e kadar kuyruk arkası (sıralı serve korunur).
- Doğrulama: `cargo test`, `cargo fmt --check`, `python3 tests/test_pybridge.py`, `node --test tests/breaks_add.test.js tests/m3_fixes.test.js tests/m4_fixes.test.js`, `cargo build` + dört adaptörde canlı `add→remove→breaks` + `stops.json` parsed-değer diff + browser reload-sonrası-kaldırma + silinmiş-dosya kaldırma + `close confirmed`. Yeni unit+canlı testler şart; bu turda koşulmuyor.
- Gate/stop/rollback: tutarsız `breaks`/`hits`/atomiklikte STOP; rollback subkomutları kaldırır.

## Milestone M-T: Ortak Hedef-Yönlendirme Sözleşmesi (FROZEN — Batch2'ye hazır)

- Bağımlılık: M0 (donduruldu) + M2 (kaldırma kimliği). M3/M4 Batch2'de implemente eder.
- İçerik (FROZEN):
  - `targets` yanıtı (frozen alanlar): `{"ok":true,"targets":[{id, kind: main|child|worker, pid (sayı|null), state: running|stopped|exited|ignored, lastStop (nesne|null), observed (M-I nesnesi|null), scope: global|inherited|target}],"selected":"<id>","ignored":N,"droppedExited":N}` (`inherited` = global intentin o hedefteki plant kopyası). KISS: `status`/`session.json` main-odaklı ve değişmez (kalıcı `selectedTarget` alanı YOK); seçili/son-stop bilgisini `targets` taşır.
  - Yönlendirme: her komut yanıtı hizmet verilen `"target": "<id>"` kimliğini yansıtır (sessiz yeniden-yönlendirme YOK). `target`siz komut son-stop-eden canlı hedefe gider, yoksa main'e; main hiç stop etmemiş child-only park bölümünde bu kural doğal olarak park halindeki child'a gider ve yanıt kimliği söyler. `breaks` (liste) tüm hedefleri toplar.
  - İlk-launch başarısı: `--timeout` içinde herhangi hedefte ilk stop başarıdır; startup yanıtı `"target"` içerir (kalıcı `selectedTarget` yazılmadan).
  - Intent/miras: startup `--break` globaldir; gelecek hedeflerde plant denenir (`inherited`, hedef-başına `pending`→`verified`). `breaks add --target X` hedefe-özeldir, miras kalmaz, oturum-efemerdir (yalnızca global intent `stops.json`'da — Batch1 KISS korunur).
  - Hedefli kaldırma: `remove --target X` yalnızca X'teki `scope:target` kayıtlarla eşleşir (yoksa `missing`). Hedefsiz `remove`: global intent + tüm canlı hedeflerdeki plant kopyalarını düşürür (Batch1 batch/confirm kuralları hedef-başına uygulanır). Bayraksız `clear`: global line intent'ler + o an plant edilmiş tüm hedef kopyaları + efemer hedef-kayıtları (tam line-break sıfırlama).
  - Kimlik: `child:<pid>`, `worker:<sessionId>`; opak yönlendirme jetonudur, güvenlik kanıtı değildir; oturum içinde yeniden kullanılmaz. Child `observed`: yalnızca `subProcessId`/connect metadata (`{pid, source:"debugpy-subProcessId"}`); argv/cwd iddiası YOK (doğrulanmadı). Worker `observed`: `workerInfo {url, type}` + debug endpoint. M-I main `observedTarget` aynen korunur.
  - Ebeveyn-exit: ebeveyn DAP oturumunun sonu child'ları `exited` yapmaz; her oturum kendi `terminated`/disconnect/exit olayıyla bağımsız uzlaştırılır. Oturum, main exited VE izlenen canlı hedef kalmadığında tam-exit sayılır. Node: main process çıkışı worker'ları bitirir → tümü `exited`. Handshake-keşif taşması `pump`'u tıkamaz: keşif penceresi `--timeout` ile sınırlıdır; geç gelenler çalışan oturuma olayla bağlanır; close sonrası olaylar yoksayılır.
  - Sınırlar (frozen): max **8 aktif non-main hedef** + **16 girdilik exited geçmişi** (`droppedExited` sayacı).
  - Taşma-bırakma (uygulama ASLA asılı kalmaz; Batch2 canlı-kanıtla düzeltildi):
    - M0'daki "child-disconnect öldürmez" gözlemi EKSİKTİ: debugpy 1.8.21'de
      HERHANGİ bir child DAP bağlantısını kapatmak (DAP `disconnect` VE ham
      socket close; yarım-handshake'te adaptör prosesi exit 1 ile ölür, tam
      handshake'te yalnızca o child-oturum kapanır) ana oturumu riske atar.
      Kanıt: adapter/components.py `Component.disconnect → session.finalize`,
      adapter/sessions.py `_finalize` hatasında `os._exit(1)`; canlı repro:
      ilk child kapatılınca adaptör 2 sn içinde öldü (rc=1, port refused).
      Ayrıca `-c`/`<string>` stack probu YANLIŞLAR: spawn_main worker'ları
      import sırasında `-c` görünür; prob gerçek worker'ı helper sanıp
      (plant'li!) terk etti → worker ilk hit'te sonsuz park → ağaç kilitlendi.
    - Geçerli mekanizma (implemente): `resource_tracker` daemon'ı
      exec-zamanı cmdline'dan tanınır (`/proc/.../cmdline` veya `ps`;
      `resource_tracker` alt-stringi; stack-prob YOK) → HİÇ bağlantı açılmaz
      (`helpersReleased`, soket/bütçe yok; erken server'ı askıya alınmadığı
      için özgür koşup kendi çıkar — tüm mp koşuları doğrular). Bütçe-üstü
      GERÇEK child'a MİNİMAL handshake (`initialize` → verbatim `attach` →
      `setBreakpoints` YOK → `configurationDone` → yanıt drene) SONRA
      kurulmuş-oturum raw-close (Session[2] emsaliyle contained) + soketsiz
      `ignored` kaydı: handshake'siz server SONSUZA askıda kalır (canlı
      kanıt: pydevd authorize sonrası susar, `sleep(6)` 70 sn koşmaz) —
      configure + breakpointsiz child ise asla park edemez, özgür koşup
      çıkar. Başarısız handshake'ler bounded `retired` listede AÇIK tutulur
      (16; yalnızca overall-cleanup kapatır); liste dolunca yeni child'lar
      connect-ÖNCESİ ignored sayılır (yalnızca 16-ardışık-başarısızlıkta
      ulaşılır; belgeli rezidüel risk). İzlenen child soketi exit sonrası
      history'de KAPATILIR (kurulmuş-oturum, contained). Soket sahipliği:
      in-flight ≤1 + izlenen ≤8 + retired ≤16.
    - Node ignored worker (`waitingForDebugger`): TEK yardımcıdan
      (`kickWorkerFree`) sarmalı `Debugger.resume` + `Runtime.runIfWaitingForDebugger`
      İKİSİ birden gönderilir (fire-and-forget; gate'te takılıya gate, parkalıya
      resume çözer — teki yetmez), izleme bırakılır (`ignored`); sonraki pause
      olayları AYNI yardımcıyla otomatik-kick edilir, park edilmez. (Node
      tarafında paylaşımlı-oturum finalize riski YOKTUR; worker bağımsızdır.)
  - `close`: launch kendi ağacını/process'ini biçer (py parent `disconnect {terminateDebuggee:true}`; node process kill); attach detach eder. `close` yanıtı mevcut `confirmed` şeklini korur.
  - Mekanizma (M0-frozen, TEK): Python `debugpyAttach` + child-başına DAP (gövde-verbatim attach, yanıt `configurationDone` sonrası); Node `NodeWorker` sarmalı (`Target.*` implementasyonda YOK — fallback yok). Attach tek-hedef kalır; pytest-altı child kapsam dışı (UNVERIFIED).
- Kabul: Batch2 implementasyonuna hazır. Kod: M3+M4 (sıralı).

## Milestone 3: Python Child (M-T frozen; Batch2'de M4'ten önce implemente edilir)

- Taşıma (frozen): AYNI adaptör portuna ikinci eşzamanlı DAP bağlantısı; `initialize` → `attach` (debugpyAttach gövdesi verbatim) → `setBreakpoints` (global miras + hedefe-özel) → `configurationDone`; `attach` yanıtı `configurationDone` SONRASI beklenir (önce beklemek deadlock — M0). Çıplak `subProcessId` attach YOK. Opt-in kapalıysa launch'a açık `subProcess:false` basılır (M0; hedefe enjeksiyon değildir). `attach` child yönlendirmesi kapsam dışı (olay gözlense bile).
- Batch2 notları: child kaydı `{id: child:<subProcessId>, pid, dap-conn, state, plant, observed:{pid, source:"debugpy-subProcessId"}}`; helper tespiti stack-prob DEĞİL exec-cmdline (`resource_tracker`, `/proc`/`ps`, bağlantısız bırakma); bütçe-üstü gerçek child'a MİNİMAL handshake (breakpointsiz) + kurulmuş-close + `ignored` kaydı (handshake'siz askı kanıtlı); başarısız handshake bounded-retired AÇIK (16, yalnızca cleanup kapatır); izlenen exit history'de kapatılır (contained); soket sahipliği in-flight ≤1 + izlenen ≤8 + retired ≤16; parent `disconnect {terminateDebuggee:true}` ağacı biçer; parent-exit child state'ini değiştirmez (bağımsız uzlaşma).
- Kapsam: `--subprocess` opt-in (varsayılan kapalı) + M-T `targets`/`target`/`scope` + global miras + `maxTargets` + launch `close` sahipliği. Dosyalar: `src/cli.rs`+`src/spawn.rs` (`Target::PyLaunch`), `src/session.rs` özet, `pybridge.py` hedef tablosu/olay/`dispatch`.
- Geriye uyumluluk: `target`siz = main; `status`/`stops.json` ek-alanlı.
- Doğrulama: yeni DAP-olay unit'leri + canlı main+child plant/park/close + sınır/`warning` + attach-keşifsizliği negatifi + `close confirmed`. Koşulmuyor (plan).
- Stop: handshake deadlock'u / plant ıskalama / bırakılamayan child durumunda STOP. Rollback: bayrak+tablo kalkar.

## Milestone 4: Node Worker (M-T frozen; Batch2'de M3 sonrası implemente edilir)

- Taşıma (frozen, TEK mekanizma; `Target.*` fallback YOK): main resume ÖNCESİ `NodeWorker.enable {waitForDebuggerOnStart:true}` (bayraksız) → `attachedToWorker` → sarmalı plant+komut (`sendMessageToWorker {sessionId, message:<stringified-CDP>}` / `receivedMessageFromWorker`); çıplak `sessionId` alanlı `Debugger.*` çağrısı YOK (yanlış oturuma gider — M0). Hit hükmü `hitBreakpoints` ile verilir (`reason:'other'` dahil). Exit `detachedFromWorker` ile uzlaştırılır; main çıkışı tüm worker'ları `exited` yapar. `attach` worker keşfi kapsam dışı.
- Batch2 notları: worker kaydı `{id: worker:<sessionId>, ...}`; worker-oturuma ÖNCE sarmalı `Debugger.enable` (+ `Runtime.enable`) ŞART (olmadan plant yanıtı gelir ama OLAY AKMAZ — canlı kanıt); plant+resume YETMEZ, `Runtime.runIfWaitingForDebugger` da gerekir (canlı kanıt: resume-tekli worker hiç script parse etmedi); geç gelen worker'da bütçe varsa plant+resume, yoksa resume+bırak (sonrası otomatik-resume, park yok); URL eşleşmesinde `/var`→`/private/var` toleransı (M0).
- Kapsam: opt-in worker takibi (varsayılan main-only) + M-T `targets`/`target`/`scope` + worker script plant/resolve/slide + hedef-bazlı park + exit. Dosyalar: `nodebridge.js` (hedef tablosu, auto-attach, olay dalları, `dispatch`), `cdp_conn.js` yalnızca M0 gerektirirse.
- `close`: launch process kill worker'ları biçer (aynı process); attach detach.
- Doğrulama: yeni CDP-yönlendirme unit'leri + `examples/node-demo/worker.js` tabanlı canlı main+worker plant/park/close + exit negatifleri. Koşulmuyor (plan).
- Stop/rollback M3 ile aynı.

## Milestone 5: Eşzamanlılık (dört köprü; gate M-I + M2 + dondurulmuş M-T metni)

- Bağımlılık: M2 + M-T metni dondurulduktan sonra. Farklı-hedef paralelliği maddeleri GO olan kola koşulludur; iki kol da kesin NO-GO ise M5 dört adaptörde tek-hedef eşzamanlılıkla ilerler (M3/M4 bilinçli sıralıdır).

- Sınıflandırma (kesin, sezgisel saflık deneyi YOK):
  - Kare-bağlı okumalar (`context`/`vars`/`stack`/`eval`) ve `step` parksızken fail-fast (`no stopped thread`) — eski kareler asla güncel gibi sunulmaz; v1'de kirli-işaretli (stale) snapshot modu YOKTUR (reddedildi: karmaşıklık).
  - `eval` asla güvenli-okuma değildir (getter/fonksiyon mutate edebilir; Java senkron-çağrı emsali).
  - Canlı okumalar (`threads`/`breaks`/`logs`) parksız cevaplanabilir (mevcut dengeli-suspend/`verifyTab`/halka korunur).
- Zamanlama: varsayılan sıralı korunur (tek istemci etkilenmez). İkinci rakip resume (`continue`/`step`/`reload`) veya resume-sırası breakpoint mutasyonu (`add`/`remove`/`clear`) → `busy` reddi (`{ok:false, error:"busy: <cmd> outstanding …"}`); sessiz kuyruk kalkar (SKILL güncellenir). Farklı hedeflerdeki rakipler yalnızca ilgili M3/M4 kolu GO ise bağımsız koşabilir.
- `close`: bekleyen resume'u sınırlı bekler (mevcut 65 sn tolerans), sonra türüne göre biçer (launch) / detach eder (attach); yabancı hedef öldürülmez.
- İptal: kullanıcı-cancel v1'de ertelidir (ayrı `cancel` yok; iptal = `close`, belgeli). İstemci kopuşu bekleyen resume'u bozmaz (yanıt düşer, durum `session.json`'a yayınlanır; mevcut best-effort yazma korumaları).
- Korelasyon: yeni multiplex/istek-kimliği YOK (bağlantı zaten koreler); operasyon-kimliği, somut cancel kullanımı olmadığı için v1 dışı.
- Sınırlar: dinleme kuyruğu mevcut backlog; snapshot klonu yalnızca yayınlanmış güncel park (tarihçe yok); ikinci rakip kuyruklanmaz (reddedilir) — sınırsız kuyruk yok.
- Dosyalar: dört köprü `serve`/`dispatch` (+ browser `reload` etkileşimi), `src/session.rs` `close` yolu, SKILL. `client.rs`/`dap.rs` protokol değişikliği yok.
- Doğrulama: `cargo test`, `cargo fmt --check`, `python3 tests/test_pybridge.py`, tüm `node --test`, `cargo build` + `python3 tests/test_live.py` (dört adaptör) + resume-sırası-canlı-okuma ve rakip-`busy` senaryoları (dört dilde). Yeni testler şart; koşulmuyor (plan).
- Stop: park kararlılığı bozulursa STOP. Rollback: eşzamanlı-okuma dalları kapatılır, sıralıya dönülür.
- M5 uygulama notu (Batch3): dört köprüde implemente edildi — py thread'li sınırlı serve (8 handler, tek `_gate` + bağlantı-başı DAP `mu`, `_auto_target` damgası), node/browser eşzamanlı serve (8 handler + 16 kuyruk, node'da swap mutex + doğrudan canlı okumalar), Java sabit havuz (8 daemon thread, tek `sessionLock`, tek event-queue tüketicisi). `src/session.rs`/`client.rs` değişikliği gerekmedi (busy CLI'dan verbatim geçer, `stamp_main` korunur). Testler: `tests/test_pybridge.py` +12, `tests/m5_concurrency.test.js` (12), `tests/M5JavaCheck.java` (19), `tests/test_m5_live.py` (6 canlı senaryo). Canlı `targets` java/browser'da CLI-tarafı roster olarak kalır (köprü protokolü yok).

## Koşullu Yüzey Sözleşmeleri

- Harici: debugpy/CDP/NodeWorker (yalnızca M0/M3/M4/M5; M1/M2 harici varsayım taşımaz).
- Şema: `stops.json` ek-alanlı (modül hedefi, eksiltme, global intent); `session.json` şekli değişmez, M-I redakte `observedTarget` için additive genişler (bozucu değişiklik yok; eski okuyucular bilinmeyen/eksik alanı tolere eder); `status` çıktısı additive. M-T kuralı korunur: kalıcı `selectedTarget` YOKTUR. Eski dosyalar null-tolerant.
- Yönlendirme: `target` yalnızca M3/M4; yokluk = seçili/main. `attach` keşfi yok.
- Kalıcılık: migrasyon yok; `close` tür-bağımlı (launch biçer/attach detach).
- Güvenlik: `check_name`/`check_dir_real` değişmez; `target` yol yorumlanmaz; `eval` uyarısı korunur; frame/secret sınırları korunur.
- UI/görsel: yok.

## Final Doğrulama ve Review

- Review düzeltmelerinden sonra: `cargo test`, `cargo fmt --check`, `cargo build`, `python3 tests/test_pybridge.py`, `node --test tests/breaks_add.test.js tests/framing.test.js tests/m3_fixes.test.js tests/m4_fixes.test.js tests/stoptimeout.test.js`, repo desenini yeniden kullanan Java derleme kontrolü (`javac -d <görev-sahipli-geçici-dizin> bridge/java/src/*.java tests/M4JavaCheck.java`; yalnızca derleme, çalıştırma yok) + `python3 tests/test_live.py` (dört adaptör canlı süiti, izole `HOME`; ağ kurulumu yok).
- Review: bu plan için **FULL analyzer plan review** + milestone-başına implementasyon review (M0 gate'i, M-T sözleşmesi ve M5 `serve` değişikliğine ek dikkat).
- PASS yeniden kullanımı: aynı diff/ortam PASS'ı tekrar koşulmaz; köprü/taşıma değişirse ilgili canlılar yeniden koşulur.
- Mevcut süitler regresyon içindir; her milestone'un yeni davranışı için açık yeni testler (unit + canlı) şarttır. Bu turda test çalıştırılmadı.

## Riskler ve Rollback

- debugpy şema sürüklenmesi → M0 sürüm-pinned prob + `doctor` kanıtı; rollback M3'ü kapatır.
- Node mekanizma/flag matrisi → M0 + belgeli opt-in; rollback M4'ü kapatır.
- Park kararlılığı → M5 en son + klon izolasyonu + fail-fast; rollback sıralıya döner.
- Kaldırma kimliği → saklı-kimlik + kısmi-başarı + atomik kalıcılık; rollback subkomutları kaldırır.
- Kaynak → `maxTargets` + `ignored` sayaç + sahipli `close`; rollback opt-in'leri kaldırır.
- Kimlik sızıntısı (ham secret persist/log veya redaksiyonsuz argv) → M-I STOP; rollback ek-alanları kaldırır (eski okuyucular etkilenmez).

## Birim/Canlı Kabul Matrisi

| Yetenek | Birim (yeni) | Canlı (yeni + mevcut süit) |
|---------|--------------|----------------------------|
| Modül launch | CLI/spawn parse + `target_summary` | test-gövdesi breakpoint + args/cwd/interpreter/negatif fixture |
| Hedef-kimliği (M-I) | redaksiyon/truncation/no-env + PID pozitif/yok/uyumsuz | aynı-dosya yanlış-process fixture + browser URL/title + no-secret taraması + attach lifecycle |
| Kaldırma (4 adaptör) | clear arg-reddi + eksiltme (silinmiş-dosya dahil) | 4 dilde tur + `stops.json` parsed-değer diff + reload-sonrası (browser) |
| Ortak sözleşme | — (metin) | review (kod yok) |
| Python child | DAP olay/yönlendirme | main+child + sınır + negatifler |
| Node worker | CDP yönlendirme | main+worker + exit negatifleri |
| Eşzamanlılık | serve/reddet birimleri | `test_live.py` + okuma/`busy` senaryoları (4 dil) |

## Ertelenen Sınırlar (v1 dışı)

- `attach` child/worker keşfi; `breaks clear` kapsam genişletmesi (v1: bayraksız, yalnızca line-break) ve canlı logpoint/watch/exit `add`; process envanteri/seçim UI (picker yok); hedef-kodda eval/import ile metadata; env toplama; kirli-işaretli (stale) snapshot modu; kullanıcı `cancel` komutu; multiplex/istek-kimliği; hedefe-özel kalıcılık (yalnızca global); yeni daemon topolojisi/bağımlılık/dosya bölme.

## Maddi Kullanıcı Kararları (öneri ile)

1. **İkinci rakip resume:** sessiz kuyruk yerine `busy` reddi. Öneri: `busy` (yarış görünür, tek-istemci etkilenmez).
2. **Hedefe-özel breakpoint kalıcılığı:** global-only (ephemeral per-target) vs tam kalıcılık. Öneri: global-only (KISS; `stops.json` şeması minimal).
3. **Parksız kare-okuma:** fail-fast error vs kirli-işaretli snapshot. Öneri: fail-fast (stale-as-current yasağı; ek mod yok).
4. **M0 kısmi-destek:** sınırlı fallback araştırması vs anında NO-GO. Öneri: sınırlı fallback (tek mekanizma yokluğu özellik-ölümü değildir).
5. **M0 kol kapanışları:** her NO-GO kolu kapanır; iki kol da kesin NO-GO ise M-T hattı kapanır, plan "desteklenmiyor" notu alır (M5 tek-hedefle ilerleyebilir). Öneri: kısmi/spekülatif implementasyon yok.

(Bayrak adları, `maxTargets` sayısı, M0/M-I prob komutları operasyoneldir — kullanıcı kararı değildir. M-I için kullanıcı kararı YOK önerilir: redaksiyon/cap/persist-kuralları güvenlik varsayılanıdır, non-negotiable.)

## Çalıştırılan Komutlar (bu revizyon)

- Read-only: bu plan dosyasının tamamı, `browserbridge.js` (parse/frag/arm/add/dispatch/serve/reload/cleanup/verifyTab), `BridgeEval.java:141-180` (gölge), `removeBreakpoint` sıfır-eşleşme grep'i, `test_live.py` test envanteri grep'i.
- Çalıştırılmayanlar: implementasyon/test/runtime/process yok (plan revizyonu).
- Değişiklik: YALNIZCA `docs/debugger-feature-roadmap.md` (bu dosya).
- M0 (ham protokol probları; repo'ya yazılmadı, throwaway'ler onaylı geçici dizinde koştu ve silindi): sürüm kanıtı `python3 --version` (CPython 3.14.6), provisioned venv `debugpy.__version__` (1.8.21), `node --version` (v26.8.1); DAP probu `python3 <tmp>/m0/dap_probe.py {popen,spawn,fork,popen_nosub,popen_exit}` + `attach_probe.py` (kavramsal; tam komutlar temp silindiği için plana gömülmedi); CDP probu `node <tmp>/m0/node_probe.js` (4 tur). Sonuçlar: Python üç dal GO + default-ON + sahiplik matrisi; Node NodeWorker GO / Target-flat NO-GO + `hitBreakpoints` atfı + detach; pytest UNVERIFIED. Temizlik: geçici dizin silindi, `pgrep` artık-süreç yok, `git status` yalnızca bu dosya (untracked `docs/`).

## Batch 1 (M1+M-I+M2) Implementation Notes

- `python -m debugpy --listen` forks: the kernel-observed attach listener PID is a child of the spawned process. Attach identity asserts the listener (debugpy argv + port), never Popen-PID equality.
- CLI-computed `--observed-target/--observed-hint` must precede the `--` program-args separator in bridge argv; anything after `--` belongs to the target.
- clap program/module conflict exits before the JSON envelope (nonzero exit, empty stdout); dispatch-level exactly-one validation covers the neither-form case.

## Workflow Feedback

- Sürtünme (doğrulanmış): düzeltme brifingindeki satır-numarası alıntıları plan düzenlendikçe kayıyor; bu turda altı düzeltme grubunun tamamı, verilen satır ref'lerine rağmen tam dosya yeniden-okuması + taze grep'ler (`browserbridge.js` serve/dispatch/reload, Java gölge yolları, `removeBreakpoint` sıfır-eşleşmesi, `test_live.py` envanteri) gerektirdi.
- En küçük sonraki-delegasyon düzeltmesi: gelecek düzeltme brifingleri satır numarası yerine değişiklik başına 5-10 satırlık tam alıntı taşısın.
- Kapsam/güven: yalnızca plan-düzenleme işleri; güven yüksek (bu turun kanıtı: tedarik edilen ref'ler tek başına hiçbir düzeltmeyi kapatmaya yetmedi, tamamı taze dosya kanıtıyla doğrulandı).

## UX Batch (wait + capture + stop diagnostics) Implementation Notes

- Contract: `wait --timeout N [--target]` is a pure long-poll (never
  resumes; immediate success when the selected target is already parked,
  else the next fresh stop; timeout preserves session/intents with
  `timeout: no stop within Ns; <hint>`). Existing one-request
  connection, timeout+5 client bound, no protocol IDs. Occupies the
  per-target outstanding slot (explicit target) so rivals busy-reject;
  live reads stay prompt; frame reads fail while running; close accepted.
  Omitted target: acceptance-time auto target for immediate parked,
  first fresh stop on any target otherwise (response stamps actual).
- `capture --timeout N --pause-budget MS [--target] [--break SPEC]
  [--frames K --vars K]` plants an optional line-only ephemeral
  target-scoped break (same parser, no stops.json, no inheritance),
  collects a bounded snapshot on a prepark WITHOUT resuming
  (`targetWasPaused:true,resumed:false`), else waits fresh, collects,
  REMOVES THE EPHEMERAL BEFORE RESUME, and auto-resumes within budget
  (overrun still resumes, then reports `budgetExceeded`). Collection or
  removal failure still resumes; timeout never resumes (nothing parked)
  but still removes the ephemeral; client disconnect drops only the
  response (bridge completes cleanup+resume, state published). No eval,
  no persisted vars. Defaults: budget 2000ms (max 10000), timeout
  1..3600, frames 1..10, vars 1..20. adapters/guarantee: Java
  awaitStop-without-resume, py pump-without-resume, node/browser
  pump-without-resume; one event reader / Batch3 locks / outstanding
  preserved. Browser capture does not survive reload.
- Diagnostics are additive on every stop-return/context/capture:
  session-monotonic `stopId`, `parkedAtMs`, stopping thread `{id,name}`,
  target, reason, native-or-null `hitBreakpoints` (never fabricated),
  requested/bound spec+line + hit count when attributable (null when
  not), `sameLocation`/`sameThread` vs the previous park,
  `elapsedSincePreviousStopMs`, capture `pauseDurationMs /
  targetWasPaused / resumed`; plus the parked `warning` (HTTP handler
  stays open until continue/capture-resume/close). Same-line immediates
  are diagnosed (loop/re-entry/other-thread/async/slide), never
  suppressed. Node/browser timeout text aligned to the typed
  `timeout: no stop within Ns` shape (py/java already had the `s`).
- Tests: `tests/test_wait_capture.py` (py 11), `tests/wait_capture.test.js`
  node+browser (18), `tests/M6JavaCheck.java` (44, no live VM), CLI
  bounds in `src/cli.rs`, and `tests/test_ux_live.py` (11 live: per
  adapter wait immediate/fresh/timeout + capture prepark/fresh/timeout
  + same-line diag; py/node/java HTTP recipe end to end; busy+close
  during wait; client-disconnect resume on py/node/java; browser
  interval capture + reload park). Checkpoint re-ran M5 live (6) and
  legacy tests 23-31 (9) green; full legacy 31 stays post-review.
- Debugging notes that bit: debugpy slides dead/unreached lines in the
  loop file onto live lines (use a never-called helper file for
  armed-but-unreached breaks); `continue` after a parked handler resumes
  at once but then waits for the NEXT stop (its timeout means
  "resumed, nothing more hit"); capture on an already-parked target
  must not resume (test sequencing must free-run first); node/browser
  bridge sources are embedded at compile time (`cargo build` refreshes
  live sessions).

## Tasarım Notu (BRAINSTORM — implementasyon yok): Endpoint–Adapter–Debuggee Ayrımı + Dürüst Capture-Timeout Nedenselliği

> Statü: **tasarım önerisi, dondurulmadı; kod/test/skill değişikliği YOK.** Bu bölüm yalnızca analiz + önerilen MVP + ertelenenleri dondurur. M-I'in genelleştirilmiş devamıdır (M-I metni değişmez). Tetikleyici: katmanlı launch (`uv run --with debugpy python -m debugpy ... -m uvicorn` benzeri) sonrası `attach.observedTarget.argv` debugpy adapter/site-packages'i gösterdi, `uvicorn`/uygulama dosyasını değil — M-I'in "yanlış-process attach'i belli etme" niyeti sarılı launch'ta zayıflıyor. Hedef: dilden bağımsız (mümkün yerde) endpoint/adapter/debuggee ayrımı + trigger'ı göremeyen debugger'ın timeout'ta yalan söylememesi.

### Kanıtlanmış Gerçekler (prob + doküman + repo)

- **F1 — debugpy `--listen` portunu ADAPTER tutar, debuggee değil.** Canlı prob (provisioned venv, debugpy 1.8.21; stdlib wrapper `exec: python -m debugpy --listen 127.0.0.1:5788 --wait-for-client srv.py`): `lsof -iTCP:5788 -sTCP:LISTEN` → PID_A, argv `.../site-packages/debugpy/adapter --for-server ...` (adapter); gerçek debuggee ayrı süreç PID_S, argv `-m debugpy --listen ... srv.py`. Yani bugünkü `attach_observed` (`src/session.rs:700-753` + `port_lookup`: Linux `803-832`, macOS `891-933`; çağrı `src/spawn.rs:329-331`) endpoint-sahibini (adapter) adlandırır, debuggee'yi değil. Kullanıcının `uv`-sarılı gözlemiyle aynı kök-neden (bir katman daha wrapper ile).
- **F2 — DAP `process` olayı attach'ta debuggee'yi söyler.** Aynı proba ham DAP istemcisiyle attach (`initialize`→`attach`→`configurationDone`): `attach` yanıtından HEMEN sonra `event/process` geldi, gövde `{name: "<...>/srv.py", systemProcessId: <PID_S>, isLocalProcess: true, startMethod: "attach"}` — `systemProcessId` adapter değil debuggee PID'idir. Spec: DAP `ProcessEvent {name, systemProcessId?, isLocalProcess?, startMethod?: launch|attach|attachForSuspendedLaunch, pointerSize?}` (Context7 `/websites/microsoft_github_io_debug-adapter-protocol`). debugpy tarafı: `debugpyAttach` gövde-verbatim attach kuralı M0'da donduruldu; adapter bağlantı-sayar, olay kök-bağlantıdan gelir (`doc/Subprocess debugging.md`, Context7 `/microsoft/debugpy`).
- **F3 — pybridge `process` olayını bugün düşürüyor.** `_handle_main_event` (`bridge/py/src/pybridge.py:2224-2317`) dalları yalnızca `debugpyAttach`/`stopped`/`continued`/`exited`/`terminated`/`output`; `process` dalı yok → `return None`. Zenginleşme noktası: `handshake_attach` (`:1322-1338`) `attach` yanıtını `configurationDone` SONRASI drene eder; `process` olayı o sırada stash'te ya da hemen sonra gelir → ilk stop/attach yanıtından ÖNCE sınırlı bekleyişle yakalanabilir (launch tarafı simetriği `_drain_launch_response`, `:1319-1320`).
- **F4 — Node `/json/list` girdisi debuggee kimliğidir ama bugün atılıyor.** Canlı prob (node v26.8.1, `--inspect-brk`): girdi `{id, type: "node", title: "<...>/sleepy.mjs", url: "file://...", webSocketDebuggerUrl}` taşır, PID taşımaz. `discoverAttach` (`bridge/node/src/nodebridge.js:957-985`) girdiyi seçip ATAR, yalnızca `webSocketDebuggerUrl` döner → protokol-onaylı debuggee kimliği çöpe gidiyor. Node'da portu V8'in kendisi tutar (arada adapter süreci yok) → OS port-sahibi launch'ta da attach'te de debuggee'nin ta kendisidir (debugpy F1'den asimetrik; tasarım bunu dürüstçe söyler).
- **F5 — Java: attach'te protokolde PID yok, port-sahibi debuggee'dir; launch'ta pid alınabilir.** `BridgeConn.java:32-48` `SocketAttach` yalnızca `hostname`/`port` alır (JDI'de attach-PID kavramı yoktur). JDWP `dt_socket` portunu hedef JVM tutar (in-process) → OS argv debuggee'nindir. Launch'ta `LaunchingConnector` + `vm.process()` non-null (`BridgeConn.java:69-92`) → `Process.pid()` ile debuggee pid'i alınabilir (araç-zinciri Java 9+ ise — H3'e bakın). Ek olarak JDI `VirtualMachine {name(), version(), description()}` standart API'dir (doğrulanması ucuz, canlı prob gerektirmez).
- **F6 — Browser zaten doğru modeldir.** `buildObservedTab` (`bridge/browser/src/browserbridge.js:585-591`) `/json/list` girdisini debuggee (tab) kimliği yapar, process iddiası yoktur. Genelleştirilecek kelime dağarcığının (`debuggee` vs `endpoint`) emsalidir; browser tarafında şema-alias dışında iş yoktur.
- **F7 — Adapter argv'sinde SECRET vardır.** F1 probunda adapter cmdline'ında `--server-access-token <hex>` görüldü → `redact_argv`/`is_secret_flag` SUBSTR listesi (`src/session.rs:521-545`, `accesstoken` dahil) bunu yakalar (kod-okuma doğrulaması; canlı redaksiyon testi MVP kabulüne yazıldı).
- **F8 — Timeout metni donmuştur.** `timeout: no stop within Ns` prefix'i `tests/test_ux_live.py:344,347`'de assert'li → timeout raporundaki her ek alan ADDITIVE olur, prefix değişmez. Mevcut rapor yapısız string'dir (`timeout_text`: py `:1168-1174`, node `:1828-1834`, browser `:1059`, Java `BridgeSession.java:663`).
- **F9 — Süreç-ağacı çıkarımı güvenilmezdir, tasarım ona dayanmaz.** F1 probunda launcher reparent sonrası `PPID=1` görüldü (nohup/disown artifaktı; genel derstir: exec wrapper'lar — `uv`, `python -m debugpy` — daemonize/reparent, PID reuse, remote host). Kural: ağaçtan debuggee/launcher İDDİA EDİLMEZ; ata-zincir en fazla ertelenmiş, düşük-güvenli, açık-etiketli iştir.

### Hipotezler (doğrulanmadı — MVP'yi bloklamaz, metinde işaretli kalır)

- **H1:** launch yolunda `process` olayı payload/timing (attach F2 probu launch'ı kapsamaz; spec her ikisini de söyler ama debugpy-launch canlı kanıtı yok).
- **H2:** `--listen ... --pid <pid>` attach varyantında `process` olayı (Context7'de CLI kalıbı doğrulandı, olay payload'u doğrulanmadı).
- **H3:** Java araç-zinciri seviyesi (`Process.pid()` için 9+; derleme kontrolü MVP acceptance'ındadır).
- **H4:** `resource_tracker` benzeri helper'ların `process` olayına etkisi (etkisizlik varsayımı; M-T'deki cmdline-tanımlama kuralı saklıdır).

### Değerlendirilen Seçenekler (tradeoff + karar)

- **O1 — `observedTarget`'ı sessizce debuggee ile overwrite:** REDDEDİLDİ. `status`/`context`/attach-yanıtı tüketicileri + `test_live.py:1459-1460` (`observedTarget.pid == proc.pid` attach beklentisi) sessiz anlam değişiminde kırılır. Kural (M-I'den devralınır): `observedTarget` BIREBIR korunur; yenilik additive alandır.
- **O2 — Süreç-ağacından `launcherChain` çıkarımı (uv → python → adapter):** REDDEDİLDİ (F9). MVP'de ağaçtan gelen hiçbir alan `protocol-confirmed` sayılmaz; zincir "ertelenmiş" bölümündedir.
- **O3 — `--trigger COMMAND` (debugger tetikleyiciyi kendisi koşsun):** DEĞERLENDİRİLDİ, ERTELENDİ. Karşı argümanlar: shell alıntılama/enjeksiyon yüzeyi, süreç sahipliği/reap/zombi, timeout kompozisyonu (`--timeout` × trigger-timeout), env/secret sızıntısı, KISS ihlali — ve zorlayıcı gerekçe yok (tetikleyici zaten debugger dışında yaşıyor: HTTP isteği, insan tıkı, agent-browser). Gelecek alternatifi (ayrı tasarım ister): operasyon-korelasyon jetonu veya debugger-sahipli çocuk koşan `capture --exec`. M5'in "operasyon-kimliği v1 dışı" kararıyla tutarlıdır.
- **O4 — (ÖNERİLEN) Additive üç-rol şeması:** `endpoint` (OS-gözlemli port-sahibi, rolü dürüst etiketli) + `debuggee` (yalnızca protokol-onaylı) + `adapter` (biliniyorsa; debugpy'de endpoint-sahibi = adapter). Eski alanlar değişmez; insan çıktısı debuggee'yi öne çıkarır.

### Önerilen MVP Tasarımı (M-ID)

- **Ortak şema (proposed, additive; `requestedTarget`/`observedTarget` değişmez):**
  ```json
  "endpoint": {"host": "127.0.0.1", "port": 5678, "ownerPid": 15298,
    "source": "os-lsof-ps", "argv": [".../debugpy/adapter", "..."],
    "role": "listener-owner (not necessarily the debuggee)"},
  "debuggee": {"kind": "process", "pid": 15292, "name": "<...>/srv.py",
    "startMethod": "attach", "source": "dap-process-event",
    "confidence": "protocol-confirmed", "observedAt": 1735689600}
  ```
  (PID'ler şematik örnektir, gerçek gözlem değildir.)
- **Confidence kuralları (non-negotiable):** `protocol-confirmed` YALNIZCA protokol alanından gelir (DAP `process`, CDP `/json/list` girdisi, `debugpyAttach.subProcessId`, `NodeWorker.workerInfo`, JDI VM özellikleri). OS port-sahibi tek başına en fazla `os-corroborated` olur — debugpy attach'te adapter olduğu için OS asla `protocol-confirmed` OLAMAZ. Kaynak yoksa `unavailable: [{field, reason}]` + `warnings: ["identity-unverified..."]` (M-I deyimi korunur). Bulunamayan rol uydurulmaz (`null` + gerekçe). `pid` tek başına kimlik değildir (reuse notu; `pid` + `startMethod` + `observedAt` birlikte okunur).
- **Dil matrisi (MVP):**
  | Dil/yol | `debuggee` kaynağı | `endpoint` kaynağı | Not |
  |---------|-------------------|-------------------|-----|
  | py attach | DAP `process` (`name`, `systemProcessId`, `startMethod`) — F2 | mevcut `observedTarget` içeriği (= adapter, F1) | çekirdek düzeltme: pybridge `process` tüketir (F3 noktası), bounded bekleyiş (attach'i geciktirmez; gelmezse `unavailable`) |
  | py launch | aynı tüketici (H1 doğrulanınca netleşir) + launcher-args korunur | CLI launcher-args + adapter-pid (biliniyorsa) | H1 MVP acceptance probudur |
  | node attach/launch | `/json/list` girdisi (`title`/`url`/`id`) — F4, artık atılmaz | OS port-sahibi (= debuggee süreci; `os-corroborated`) | port-sahibi==debuggee asimetrisi metinde açık yazılır |
  | java attach | JDI VM `name/version` + OS port-sahibi argv (`os-corroborated`) — F5 | OS port-sahibi (= hedef JVM) | protokol-PID yokluğu dürüstçe `unavailable` |
  | java launch | + `vm.process().pid()` (H3) | CLI launcher-args | derleme-seviye kontrolü acceptance'tadır |
  | browser | mevcut tab kimliği `debuggee` rolüne taşınır (additive alias) — F6 | `host:port` + `debugEndpoint` | davranış değişmez |
  | child:/worker: | M-T dondurulan `observed` aynen taşınır (`debugpy-subProcessId` / `workerInfo`) | ana oturumun endpoint'i | M-T metni değişmez |
- **UX önceliği (insan çıktısı, concise):** protokol-onaylı `debuggee` varsa İLK ve belirgin satır (`debuggee: <name> (pid <pid>, protocol-confirmed)`); endpoint-sahibi ayrı satırda (`endpoint owner: <argv…> (adapter/listener — kodunuz değil)`); `observedTarget` alanı dokunulmaz, `compact_hint` debuggee-öncelikli hale gelir. Timeout/vurulmayan-break çıktısına kompakt debuggee satırı eklenir — kök-neden iddiası YOK (`verified` plant-only kuralı korunur).
- **Güvenlik (M-I'den devralınır, non-negotiable):** redaksiyon (`--server-access-token` F7 dahil) + cap (alan ≤512, toplam ≤2KB, dizi ≤32) yeni alanlara aynen uygulanır; env ASLA; ham persist ASLA; `process`-bekleyiş sınırlı (örn. ≤3 sn; attach yavaşlamaz); remote host'ta OS rolleri daimi `unavailable`.

### Capture-Timeout Nedenselliği Tasarımı (M-TC, M-ID sonrası)

- **Çekirdek dürüstlük kuralı:** debugger dış tetikleyiciyi GÖREMEZ (harici helper isteği göndermeden ölebilir; debugger tüm timeout'u bekler ve trigger durumunu bilemez) → raporda `triggerStatus: "unknown"` VARSAYILANDIR ve metin asla "hedef kodu ıskaladı" demez. O3 (`--trigger`) ertelendiği için `unknown` dışında değer üreten mekanizma v1'de YOKTUR.
- **Additive rapor (proposed; F8 prefix'i korunur, örn. `timeout: no stop within 2s; <debuggee-hint>; trigger unknown (...)`):**
  ```json
  {"error": "timeout: no stop within 2s; ...",
   "waitReport": {"waitStartedAt": 1735689600, "waitedMs": 2000, "timeoutSecs": 2,
     "breakpointState": [{"spec": "app.py:42", "state": "verified|pending|slid", "hits": 0}],
     "triggerStatus": "unknown",
     "targetIdentity": {"debuggee": {...}, "endpoint": {...}},
     "recommendation": "verify the trigger path ran (e.g. the HTTP request reached the handler); app.py:42 is verified-but-unhit"}}
  ```
  `breakpointState` mevcut plant-kayıtlarından türetilir (yeni izleme yok); `recommendation` şablonludur, hedefe-özel teşhis uydurmaz. Başarı yanıtları değişmez.
- **KISS notu:** rapor, `wait`/`capture`/launch-ilk-stop timeout'larında aynı yapıyı kullanır; `breaks verified` = plant kabulü kuralı tekrar yazılır (çalıştırma kanıtı değildir).

### Test Stratejisi (uygulanmadı — MVP acceptance'ına yazılır)

- **Birim (yeni):** DAP `process`-tüketici (stash/parse → `debuggee`; H1 varyantları; bounded-bekleyiş zaman-aşımında `unavailable`); Node `/json/list` keeper (girdi-atılmama); additive şema (eski okuyucu toleransı; `observedTarget` bayt-koruması); redaksiyon/cap (F7 `server-access-token` dahil) + no-secret taraması (persist + log); timeout-raporu (donmuş prefix + `triggerStatus: unknown` + `breakpointState`); Java derleme-seviye kontrolü (H3).
- **Canlı (yeni; ağ kurulumu YOK, `uv` YOK):** stdlib wrapper fixture — F1 kalıbı (`exec: python -m debugpy --listen ... srv.py`): external-launch + attach → `debuggee.pid == <server-pid> != endpoint.ownerPid` + insan çıktısında `srv.py` yolu, site-packages değil. Aynı-dosya-yolu yanlış-process attach negatifi (M-I niyeti korunur). Node: `/json/list` title/url taşınması. Java: fork'lanmış JDWP hedefi attach (H3'e koşullu pid). Browser: değişmezlik. Timeout: helper-failure simülasyonu (asla-vurmayan break + kısa timeout) → raporda `unknown` + breakpoint durumu + kök-neden-yok iddiası; `test_ux_live.py:344,347` prefix assert'leri yeşil kalır.
- **Kanıt geçersizleşmesi:** debugpy minor değişirse F2 probu tekrarlanır (M0 kuralı).

### Sıra / Kabul / Stop (öneri)

- **Milestone/sıra:** M-ID (kimlik v2: `process` tüketimi + üç-rol şema + UX; M5 sonrası, paylaşılan `session.rs`/`spawn.rs`/üç köprü nedeniyle tek dilim) → M-TC (timeout raporu, M-ID sonrası). Paralel implementasyon yok (sıralı repo düzeni korunur).
- **Kabul matrisi (özet):** py-attach debuggee==server-pid canlı kanıtı + adapter-endpoint ayrımı; 4 dilde additive şema + eski testler yeşil (`test_live.py:1459-1460`, `test_ux_live.py:344,347` dahil full gate); no-secret taraması temiz; `process`-bekleyiş attach süresine ölçülür tavan eklemez.
- **Stop koşulları:** redaksiyon sızıntısı veya ham-secret persist → STOP; `process`-bekleyiş attach'i >tavan geciktirirse → STOP/redesign (bekleyişsiz stash-tarama fallback'i); OS rolünü `protocol-confirmed` sayan implementasyon → NEEDS_CHANGES; H1 ters-prob sonucu (launch'ta olay yok) M-ID'yi değil yalnızca launch-satırını kapsar (attach değeri korunur).
- **Rollback:** ek alanlar kalkar (`observedTarget` tek başına kalır; eski okuyucular etkilenmez).

### Çözülmemiş Gerçekler

- H1 (launch `process` olayı), H2 (`--pid` varyantı), H3 (Java 9+ zinciri), H4 (helper-etkisizliği).
- Remote-host attach'ta OS rolleri daimi `unavailable` kalır (tasarım kararı, prob gerektirmez).
- `uv`-özel zincir (`uv run` → yorumlayıcı → debugpy → adapter): F1'in genellemesidir; uv'ye özel prob yapılmadı ve GEREKMEZ (tasarım sarıcıdan bağımsızdır — protokol kanıtı esastır).
- Gerçek PID/secret bu bölüme yazılmadı (örnekler şematiktir); F1-F2 prob artıkları onaylı geçici dizinden silindi, süreçler temizlendi.

### Bu Notun Kanıt Envanteri (çalıştırılanlar)

- Read-only repo: `src/spawn.rs:290-334,221-283`, `src/session.rs:511-545,603-666,672-753,803-933,937-961`, `bridge/py/src/pybridge.py:1168-1174,1202-1234,1245-1338,1394-1433,2197-2317,2529-2615`, `bridge/node/src/nodebridge.js:957-985,1828-1834`, `bridge/browser/src/browserbridge.js:585-591,1059`, `bridge/java/src/BridgeConn.java:32-92`, `bridge/java/src/BridgeSession.java:663`, `tests/test_ux_live.py:344,347`, `tests/test_live.py:1459-1460`.
- Context7: DAP `ProcessEvent` (startMethod/systemProcessId), debugpy `debugpyAttach`/subprocess-attach dokümanı, CDP `Target.getTargets`/`attachedToTarget` (NodeWorker sarmalı M0 bulgusuyla tutarlı).
- Canlı problar (onaylı geçici dizin, stdlib-only; `uv`/ağ kurulumu YOK; sonrası silindi + süreç temizliği doğrulandı): (a) wrapper-exec debugpy `--listen` + `lsof`/`ps` port-sahibi kanıtı (F1+F9); (b) ham-DAP attach `process`-olay kaydı (F2); (c) `node --inspect-brk` + `/json/list` + `/json/version` (F4).

## M-ID/M-TC Implementation Notes (Batch4 — implemente edildi, review bekler)

> Kapsam: yukarıdaki Tasarım Notu'nun MVP'si (M-ID: üç-rol kimlik + M-TC: dürüst timeout raporu), tek dilimde dört adaptör + CLI. M-I/`observedTarget` metni ve tüm frozen sözleşmeler değişmez; ek alanlar additive'dir. Test/skill eklendi; commit yok.

- **Şema (additive):** `session.json` + attach/start yanıtı + `status` + `context` + `targets` roster'ında `targetIdentity: {debuggee, endpoint, adapter}` (bu sırada). Rol-başına yalnızca bilinen alanlar + `source`/`confidence`/`observedAt`/`unavailable[]`. `confidence` katıdır: `protocol-confirmed` yalnızca protokol verisinden (DAP `process`, `/json/list` girdisi, JDI VM özellikleri), `os-corroborated` yalnızca OS port-sahibinden, gerisi `unavailable` + gerekçe. `observedTarget` bayt-uyumlu korunur (`test_live.py:1459-1460` yeşil).
- **Python:** `handshake_attach`/`handshake_launch` `attach`/`launch` yanıtı + `configurationDone` sonrası DAP `process` olayını bounded (~2 sn) tüketir (önce stash taraması, sonra re-stash'leyen sınırlı okuma — stop/`debugpyAttach` kaybolmaz); geç gelen olay `_handle_main_event`'teki `process` dalıyla zenginleşir (park etmez, child akışına dokunmaz) ve `session.json`'u atomik günceller. Debuggee PID varsa `/proc`/`ps` argv/cwd/exe `osDetails` altında (`os-corroborated`, confidence yükseltilmez). Endpoint+adapter = CLI OS gözlemi (adapter argv'de `debugpy` tanınırsa; launch'ta köprünün kendi spawn pid'i). Yayın öncesi her rol re-redact + cap (alan ≤512, rol ≤2KB, toplam ≤4KB) — `--server-access-token` ham sızıntısı unit ile kapalı.
- **Node:** `discoverAttach` seçili `/json/list` girdisini artık atmaz (`attachEntry {id,title,url}`); launch'ta ws URL'den parse edilen porta best-effort `/json/list` (URL-eşleşen ya da tek girdi). Debuggee `protocol-confirmed` (PID iddiası YOK — `pid` alanı taşınmaz); endpoint host/port/ws + CLI OS pid'i (`os-corroborated`); adapter `{inProcess:true, unavailable}`.
- **Java:** attach'te JDI `vm.name()/version()` (`protocol-confirmed`, target-code eval YOK); launch'ta `+ vm.process().pid()` (JDK 24 zincirinde derlenir, try/catch korumalı); attach PID `unavailable` (SocketAttach'ta PID kavramı yok). Endpoint host/port + CLI gözleminden regex-çıkarım ownerPid/argv/exe/cwd (CLI-redacted verbatim; toplam-bütçe aşımında argv düşer + işaretlenir); adapter in-process. `StopTimeout`/`BridgeException` `waitContextJson` taşır; continue/step/idle/handshake bekleyişleri context'siz kalır.
- **Browser:** mevcut tab kimliği `debuggee` rolüne taşınır (`protocol-confirmed`, PID iddiası yok); endpoint `host:port` + `debugEndpoint` (`unavailable`, OS gözlemi yok); adapter tanımsız-süreç. `pump` artık typed `StopTimeout` atar (mesaj aynı).
- **Timeout raporu (M-TC):** `wait`/`capture` timeout'unda köprü `{ok:false, error:"timeout: no stop within Ns; …", waitContext:{waitStartedAt, waitedMs, triggerStatus:"unknown", expectedBreak?, targetIdentity, note}}` döner (prefix donmuş, F8). CLI (`BridgeFailure` + `output::emit`) `waitContext`'i JSON/human hata zarfına additive taşır (mesaj dizesi değişmez). Başarılı capture değişmez (`triggerStatus` uydurulmaz). Operasyon detayı persist edilmez (yanıt/hata dışında).
- **H-kararları:** H1 (launch `process` olayı) — CANLI DOĞRULANDI (debugpy 1.8.21: launch handshake'te `process` olayı gelir, `startMethod:"launch"` + debuggee PID; launch-satırında `protocol-confirmed` debuggee + `osDetails` zenginleştirmesi çalışır; ayrıca launcher cmdline'ındaki ikinci secret varyantı `--adapter-access-token` köprü-redaktörünce canlı yakalandı). Olay gelmezse launch-satırı `unavailable` kalır (fallback korunur), attach değeri bağımsızdır. H2 (`--pid` varyantı) — denenmedi, kapsam dışı (CLI'da pid girdisi yok). H3 (Java 9+ zinciri) — host JDK 24 ile derlenir + runtime-guard'lı; eski JDK'da derleme garantisi yok (destek matrisi M0'dadır). H4 (helper-etkisizliği) — M-T cmdline kuralı saklı, `process` olayına etkisi varsayılmadı (first-wins + child akışı dokunulmaz).
- **Kalan sınırlar:** remote-host attach'te OS rolleri daimi `unavailable`; `uv`-özel zincir prob'lanmadı (tasarım sarıcıdan bağımsızdır); PID-reuse'a karşı debuggee zenginleştirmesi okuma-anı canlılık kontrolünden ibarettir; `waitContext` `continue`/`step` timeout'larında taşınmaz (bilinçli).
- **Testler:** `cargo test` (+3: failure-passthrough, identity-cache, surface), `tests/test_pybridge.py` (+10: stash/delay/unavailable, redact F7, caps, waitContext, expectedBreak, envelope, OS-enrich), `tests/target_identity.test.js` (8: node entry-keeper/no-pid/inProcess, waitContext, expectedBreak, caps; browser tab/waitContext), `tests/M7JavaCheck.java` (37 assertion, canlı VM yok), `tests/stoptimeout.test.js` pattern-güncellemesi (typed-timeout niyeti korunur), `tests/test_ux_live.py` flow genleşmesi (prefix + unknown/expectedBreak/identity-armed). SKILL: katmanlı-kimlik + dürüst-trigger reçetesi + `uv/debugpy/uvicorn` örneği.
