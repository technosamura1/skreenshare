# screenshare — свой Parsec для двоих

Цель: стрим экрана со звуком между двумя Windows-ПК через интернет, без серверов и настроек.
Запустил exe → обменялись кодами → смотришь.

## Железо и сеть
- Хост (стримит): RTX 5070, интернет ~100 Мбит/с.
  - Мониторы: 1920x1080@143 на RTX 5070, 2560x1440@279 на AMD Radeon (встройка). Стоит Parsec Virtual Display.
- Клиент (друг): GTX 1070 → декодирует HEVC аппаратно, AV1 не умеет.
- Россия: Parsec заблокирован. Никаких центральных серверов, только прямое P2P + несколько STUN из разных мест.

## Требования MVP
- 1080p60, «визуально без потерь»: HEVC NVENC, CBR ~40 Мбит/с (настраиваемо).
- Звук хоста (WASAPI loopback).
- Только просмотр. Управление мышью/клавиатурой — позже.
- «Из коробки»: одна папка (exe + DLL FFmpeg) в zip, без установки.

## Архитектура (Rust)
- Захват: DXGI Desktop Duplication (`windows` crate), курсор дорисовываем сами
  (DD не включает курсор). Копия в staging → CPU (так работает и с монитором на AMD-встройке,
  и с NVENC на NVIDIA).
- Кодирование: FFmpeg (`ffmpeg-next` + `ffmpeg-sys-next`), `hevc_nvenc`, вход BGRA,
  preset p4, tune ull, zerolatency, без B-кадров, бесконечный GOP + IDR по запросу клиента.
- Звук: `cpal` (loopback на устройстве вывода), PCM i16 48 кГц стерео, пакеты по 10 мс. Opus — позже.
- Сеть: один UDP-сокет (`socket2`, большие буферы).
  - Свой мини-STUN-клиент → публичный адрес. Проверка симметричного NAT по двум серверам.
  - Двусторонний обмен кодами: хост даёт код `H1…`, клиент отвечает кодом `C1…`.
    Код = base64url(токен сессии u32 + кандидаты ip:port: публичный и локальные).
  - Обе стороны шлют punch-пакеты на все кандидаты, фиксируют адрес первого валидного ответа.
  - Видео режем на фрагменты ≤1200 байт: заголовок (тип, токен, frame_id, frag_idx, frag_count, keyframe).
    Пейсинг отправки (token bucket), чтобы не душить роутер.
  - Потеря кадра → клиент ждёт ключевой кадр и шлёт запрос IDR (не чаще раза в 200 мс).
  - Типы пакетов ≥0x80, чтобы не путать со STUN.
- Клиент: FFmpeg HEVC + `d3d11va`, NV12 → BGRA через swscale, окно `winit` + `softbuffer`,
  F11 — полный экран. Звук через `cpal` с джиттер-буфером (~40–60 мс, сброс при >100 мс).
- UI MVP: консоль на русском (UTF-8), коды копируются в буфер (`arboard`).
- Для автотеста на одном ПК: `--exchange-dir DIR` (обмен кодами через файлы) и `selftest`
  (захват → кодирование → декодирование, тайминги и проверка цвета).

## Сборка
- Нужно: Rust (есть), MSVC (есть), LLVM/libclang (для bindgen в ffmpeg-sys-next),
  FFmpeg shared dev build (BtbN, github.com/BtbN/FFmpeg-Builds) в `third_party/ffmpeg`.
- `.cargo/config.toml`: `FFMPEG_DIR`, `LIBCLANG_PATH`.
- Версия FFmpeg должна совпадать с версией `ffmpeg-next`.

## Статус
- [x] Инструменты: git, gh (вошёл как technosamura1), Rust 1.99, MSVC.
- [x] Разрешение на скачивание LLVM (winget LLVM.LLVM) и FFmpeg (BtbN win64-gpl-shared) получено.
- [x] LLVM установлен: `C:\Program Files\LLVM\bin\libclang.dll` (только на ПК; в CI — LLVM раннера).
- [ ] FFmpeg: выбрать сборку BtbN под версию `ffmpeg-next` (на 2026-10-24 последняя 9.0.0 / ffmpeg-sys-next 9.0.1),
      распаковать в `third_party/ffmpeg`.
- [x] MVP код (v0.1.0).
- [x] selftest: цвет ±2/255, 2560x1440@60, захват+NVENC ~5 мс, ~37 Мбит/с.
- [x] Локальный тест (хост и клиент на одном ПК, через UDP и STUN): 60 к/с, 0 потерь.
- [x] Повёрнутые (портретные) мониторы: поворот при копировании из текстуры (проверено на 1080x1920).
- [x] v0.2.0: окно-лаунчер на egui (тёмное, в стиле Parsec), стрим в дочерних процессах `host/client --ipc`
      (строки `@CODE/@CONNECTED/@STATS/@BADCODE/@ERROR`, STOP через stdin), «Проверить на этом ПК»,
      просмотр сразу на весь экран без рамок (F11/двойной клик/Esc), bicubic-масштабирование.
- [ ] Тест с другом.
- [ ] Сборка zip, позже GitHub Actions + релизы.

- [x] v0.3.0: звук без эха — process loopback (`ActivateAudioInterfaceAsync`, EXCLUDE_TARGET_PROCESS_TREE)
      исключает дерево процессов Discord/PTB/Canary/Vesktop; перепроверка каждые 3 с; если исключить нельзя,
      а Discord запущен — звук не шлём. `audiotest` доказывает исключение тоном 440 Гц (исключён 0.000, включён 0.029).
      Грабли: у windows-rs `PROPVARIANT` есть Drop → PropVariantClear освобождал наш стековый blob (heap corruption),
      держим его в ManuallyDrop.

## Известные косяки (TODO)
- **HDR (важно, отложено по просьбе пользователя 2026-10-24):** при включённом HDR в Windows картинка в стриме
  засвечена — legacy `DuplicateOutput` отдаёт плохо смапленный BGRA8. Это видит и друг. План фикса: если
  `IDXGIOutput6::GetDesc1().ColorSpace == G2084`, брать `DuplicateOutput1` в `R16G16B16A16_FLOAT` (scRGB),
  узнать «Яркость SDR-контента» через `DisplayConfigGetDeviceInfo(GET_SDR_WHITE_LEVEL)` (scale = 1000/W),
  компьют-шейдером (D3DCompile cs_5_0) перевести в SDR: *scale, мягкий roll-off выше ~0.8, sRGB-гамма,
  запись в RGBA8 с перестановкой каналов в BGRA-порядок → дальше как сейчас (staging, поворот, курсор).

## Потом
- Управление мышью/клавиатурой/геймпадом, Opus, шифрование (ChaCha20-Poly1305, ключ в коде),
  intra-refresh вместо IDR, FEC, выбор монитора, GUI вместо консоли, сервер-«сводник» по желанию.
