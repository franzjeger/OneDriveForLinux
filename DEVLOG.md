# OneDrive for Linux — Devlog

---

## 2026-03-22 — Local watcher synkronisering ikke-fungerende

### Problem
Etter at daemonen startet, ble ikke lokale filendringer lastet opp til OneDrive. "Local watcher started" dukket opp i loggene, men ingen "Local change detected" ble noensinne logget — selv om daemonen kjørte normalt og delta-synk fungerte.

### Diagnostikk
1. Bekreftet at inotify-watches var satt opp korrekt (2 watches: `/home/frank/OneDrive` og `/home/frank/OneDrive/Attachments`).
2. La til debug-logging i notify-callbacken → bekreftet at inotify faktisk **genererte** events.
3. La til debug-logging i `local_watcher_loop` → bekreftet at loopen **mottok** events fra kanalen.
4. Men ingen opplasting skjedde. Identifiserte tre separate bugs.

### Bug 1 — Debouncer erstattet actionable event med Access-event
**Fil:** `crates/sync-engine/src/watcher.rs` — `EventDebouncer::feed()`

inotify genererer denne sekvensen for en ny fil:
```
Create(File) → Modify(Metadata(Any)) → Access(Close(Write))
```

`feed()` brukte `HashMap::insert()` og erstattet alltid det lagrede eventet med det **siste**. Etter alle tre events var bare `Access(Close(Write))` igjen i debounceren. `is_create_or_modify()` matcher ikke `Access`, så ingen opplasting ble trigget.

**Fix:** Bytte til `entry().or_insert_with()` — behold **første** event per sti, men oppdater alltid tidsstempelet for å forlenge debounce-vinduet ved event-burster.

### Bug 2 — `should_ignore_event` kastet rename-events med temp-kildefil
**Fil:** `crates/sync-engine/src/watcher.rs` — `should_ignore_event()`

notify på Linux slår `IN_MOVED_FROM` + `IN_MOVED_TO` sammen til ett `Modify(Name(Both))`-event med `event.paths = [kilde.tmp, mål.docx]`. Den gamle koden returnerte `true` (ignorer) hvis **noen** sti matchet et ignore-mønster. Siden `kilde.tmp` matchet `.tmp`-suffikset, ble hele eventet — inkludert den ferdige `.docx`-filen — forkastet.

Dette rammet alle filer som lagres via atomic rename (Word, Excel, og andre apper).

**Fix:** Bruk `.all()` i stedet for early-return — ignorer bare eventet hvis **alle** stier matcher et ignore-mønster.

### Bug 3 — `.kate-swp` manglende fra ignore-listen
**Fil:** `crates/sync-engine/src/watcher.rs` — `IGNORED_SUFFIXES`

Kate editor lager swap-filer med suffikset `.kate-swp`. Disse ble ikke filtrert og ble lastet opp til OneDrive.

**Fix:** La til `.kate-swp` i `IGNORED_SUFFIXES`.

### Bonus — Bedre robusthet for local_watcher_loop-tasken
**Fil:** `crates/sync-engine/src/engine.rs`

`local_watcher_loop` brukte enkel `tokio::spawn` uten panic-catching. Hvis tasken krasjet (panic eller channel lukket), skjedde det silently — ingen logg, ingen restart. Remote watcher hadde allerede double-spawn+await mønsteret.

**Fix:**
- Lagt til double-spawn+await restart-loop (som remote_watcher) med logging ved crash/panic/normal exit
- Lagt til `warn!`-logg når `Ok(None)` (channel lukket) inntreffer

### Resultat
- Nye filer oppdages og lastes opp ✓
- Endringer på eksisterende filer (txt, odt, docx, xlsx) oppdages og lastes opp ✓
- Slettinger synkroniseres ✓
- Kate swap-filer, `.tmp`, `~$*`, `.~lock.*` etc. filtreres korrekt ✓

### Gjenstående
- Rydde opp `.kate-swp`-fil som ble lastet opp til OneDrive før fiksen
- Upload-side konfliktdeteksjon (etag-sjekk før opplasting)
- `sync_folders`-filter for `handle_local_event` (nå lastes alle filer i `~/OneDrive/` opp, ikke bare `sync_folders`)
- Systemd auto-start ved innlogging (ukjent årsak til sporadisk feil)

---

## 2026-07-30 — Stor kvalitets- og robusthetsgjennomgang

Full gjennomgang av hele kodebasen over 9 PR-er (+6 dependency-PR-er), alle merget med grønn CI:

### Kritiske feil fikset
- **Datatap 1:** `prepare_mountpoint` slettet alt innhold i sync-katalogen ved bytte til on_demand — flyttes nå til `.pre-fuse-backup` i stedet.
- **Datatap 2:** `reconcile_deleted_items` slettet lokalt opprettede filer (`_local_`-ID-er) med ventende opplasting etter full delta-synk.
- **Datatap 3 (potensielt):** ekskluderingsmønstre var case-sensitive (`thumbs.db` traff ikke `Thumbs.db`).
- **Token-rotasjon:** samtidige token-refresh kunne ugyldiggjøre refresh-tokenet (Microsoft roterer dem) — nå serialisert bak mutex.
- **Minne:** opplasting av store filer leste hele filen inn i RAM — nå chunk-strømming fra disk.
- Sårbare `rustls-webpki`-versjoner fjernet (ubrukt `oauth2`-crate droppet helt).

### Ny infrastruktur
- CI (fmt + clippy -D warnings + test), sikkerhetsaudit (cargo-audit), Dependabot, release-workflow (tag → GitHub Release).
- 50 tester (fra 0): enhets-, delta-integrasjons- og mock-HTTP-tester (wiremock).
- QuickXorHash-verifisering av alle nedlastinger (warn-only inntil feltvalidert).
- FUSE `forget()` med lookup-telling — inode-tabellen vokser ikke lenger ubegrenset.
- `rmdir` implementert; `readdirplus` rapporterer riktige attributter.
- Engine splittet i moduler (`engine/{mod,delta,local,pin}.rs`).

### Gjenstående (fra forrige logg + nytt)
- Feltvalidering av QuickXorHash mot ekte OneDrive → oppgrader til hard-fail.
- Upload-side konfliktdeteksjon (etag-sjekk før opplasting) — fortsatt åpen.
- `sync_folders`-filter for `handle_local_event` — fortsatt åpen.
- Dogfooding mot ekte OneDrive før v0.1.0-tag.
