# Skreenshare проектор — устройство

Цель: экран и звук Windows-ПК → дешёвый Android-проектор по Wi-Fi/LAN, без кодов, задержка 100–200 мс.

## Схема
ПК (Rust, `src/`): DXGI Desktop Duplication → (масштаб) → NVENC H.264 (low-latency, без B-кадров, IDR раз в 2 с)
→ UDP-пакеты ≤1200 байт; звук WASAPI loopback → PCM s16 stereo, пакеты по 5 мс.
Проектор (Kotlin, `android/`): UDP → сборка кадров → MediaCodec (low-latency) → SurfaceView; звук → AudioTrack.

## Протокол (`proto/` на Rust ↔ `Proto.kt`, общие тесты с эталонными байтами)
Заголовок: magic 0xD7, тип, session u32. Порт приёмника UDP 47800, порт поиска на ПК 47801.
HELLO/PROBE (поиск), START/START_ACK, VIDEO (ts, frame_id, idx, count, flags), AUDIO (seq, ts, PCM),
KEYREQ (запрос ключевого кадра при потере), PING/PONG, REPORT (статистика), BYE.
Поиск: приёмник шлёт HELLO broadcast + mDNS `_skreenshare._udp`; ПК шлёт PROBE на broadcast и на найденные по mDNS адреса.

## Задержка
- Без B-кадров, `zerolatency`, CBR, SPS патчится (`proto::h264`): num_reorder_frames=0, чтобы слабые декодеры не копили кадры.
- Приёмник: очередь декодера 3 кадра, при отставании кадры выбрасываются; аудио-джиттер-буфер 40 мс (макс. 150).
- Потеря пакета → кадр отбрасывается → KEYREQ (не чаще 150 мс) → ждём ключевой кадр.
- Отправитель: paced sender, при переполнении очереди кадры дропаются и форсится IDR.

## Не сделано / идеи
- (v0.2 сделано) A/V-синхронизация: общие метки времени, звук — главные часы на приёмнике; адаптивный битрейт по REPORT/RTT (`proto::rate`). Не проверено на железе.
- Шифрование/PIN; несколько приёмников; управление.
- Сжатие звука (Opus); HEVC для проекторов, которые его тянут.
