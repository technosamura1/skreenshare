# Установка Skreenshare проектор на Android-проектор по Wi-Fi через adb.
# Запускать двойным щелчком по install-on-projector.bat (он вызывает этот файл).
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8 } catch {}
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

$Package  = 'ru.skreenshare.proektor'
$PtUrl    = 'https://dl.google.com/android/repository/platform-tools-latest-windows.zip'
$Here     = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
$LocalDir = Join-Path $env:LOCALAPPDATA 'skreenshare-proektor'

function Say($t, $c = 'Gray') { Write-Host $t -ForegroundColor $c }

function Show-Help {
    Say ''
    Say '=== Как включить отладку по сети на проекторе ===' Yellow
    Say '1. На проекторе: Настройки -> О проекторе (О системе / Об устройстве).'
    Say '2. Найдите пункт "Номер сборки" и нажмите на него 7 раз пультом (кнопка ОК).'
    Say '   Появится надпись "Вы стали разработчиком".'
    Say '3. Вернитесь в Настройки -> Для разработчиков (иногда внутри "Система").'
    Say '4. Включите "Отладка по USB" и, если есть, "Отладка по сети" / "ADB по сети" /'
    Say '   "Беспроводная отладка" (на некоторых проекторах отдельного пункта нет —'
    Say '   тогда достаточно "Отладки по USB").'
    Say '5. Проектор и компьютер должны быть в ОДНОЙ сети Wi-Fi.'
    Say '6. IP проектора: Настройки -> Сеть -> Wi-Fi -> ваша сеть (или в приложении'
    Say '   "Skreenshare проектор" он показан на экране).'
    Say '7. Запустите этот файл снова. Когда на проекторе появится окно'
    Say '   "Разрешить отладку по USB/ADB?" — поставьте галочку "Всегда разрешать" и нажмите "ОК".'
    Say ''
    Say 'Если проектор не принимает подключение по сети: подключите его к компьютеру USB-кабелем,'
    Say 'выполните один раз в этой папке  adb tcpip 5555  и повторите попытку.' DarkGray
    Say ''
}

function Fail($msg, $help = $true) {
    Say ''
    Say "ОШИБКА: $msg" Red
    if ($help) { Show-Help }
    exit 1
}

# ---------- 1. adb ----------
function Find-Adb {
    $cands = @(
        (Join-Path $Here 'adb.exe'),
        (Join-Path $Here 'platform-tools\adb.exe'),
        (Join-Path $LocalDir 'platform-tools\adb.exe')
    )
    foreach ($c in $cands) { if (Test-Path -LiteralPath $c) { return $c } }
    $cmd = Get-Command adb.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

$adb = Find-Adb
if (-not $adb) {
    Say 'adb.exe не найден. Скачиваю официальные platform-tools от Google (около 7 МБ)...' Cyan
    $zip = Join-Path $env:TEMP 'platform-tools-latest-windows.zip'
    try {
        if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
        Invoke-WebRequest -Uri $PtUrl -OutFile $zip -UseBasicParsing
        New-Item -ItemType Directory -Force -Path $LocalDir | Out-Null
        Expand-Archive -LiteralPath $zip -DestinationPath $LocalDir -Force
        Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    } catch {
        Say ''
        Say "Не удалось скачать platform-tools: $($_.Exception.Message)" Red
        Say 'Проверьте интернет. Либо скачайте вручную' Yellow
        Say "  $PtUrl" Yellow
        Say 'распакуйте и положите папку platform-tools рядом с этим файлом, потом запустите снова.' Yellow
        exit 1
    }
    $adb = Find-Adb
    if (-not $adb) { Fail 'после скачивания adb.exe не найден.' }
    Say "Готово: $adb" Green
}

function Adb {
    # Возвращает весь вывод adb (stdout+stderr) одной строкой.
    $out = & $adb @args 2>&1 | ForEach-Object { "$_" } | Out-String
    return $out.Trim()
}

# ---------- 2. APK ----------
$apk = $null
foreach ($n in @('skreenshare-proektor-compat.apk', 'skreenshare-proektor.apk')) {
    $p = Join-Path $Here $n
    if (Test-Path -LiteralPath $p) { $apk = $p; break }
}
if (-not $apk) {
    $any = Get-ChildItem -LiteralPath $Here -Filter '*.apk' -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($any) { $apk = $any.FullName }
}
if (-not $apk) { Fail 'рядом с этим файлом нет APK (skreenshare-proektor-compat.apk). Распакуйте весь zip целиком.' $false }
Say "APK: $(Split-Path -Leaf $apk)" Gray

# ---------- 3. IP ----------
Say ''
Say 'Введите IP-адрес проектора в вашей сети Wi-Fi (например 192.168.1.57).' Cyan
Say 'Порт по умолчанию 5555; другой порт можно дописать через двоеточие: 192.168.1.57:5555' Gray
$target = $null
for ($i = 0; $i -lt 3 -and -not $target; $i++) {
    $ip = (Read-Host 'IP проектора').Trim()
    if ($ip -match '^(\d{1,3}\.){3}\d{1,3}(:\d{1,5})?$') {
        $target = if ($ip -match ':') { $ip } else { "${ip}:5555" }
    } else {
        Say 'Не похоже на IP-адрес. Нужны четыре числа через точку, например 192.168.1.57.' Yellow
    }
}
if (-not $target) { Fail 'IP-адрес не введён.' }

# ---------- 4. connect ----------
$null = Adb start-server
Say "Подключаюсь к $target ..." Cyan
$connected = $false
for ($try = 1; $try -le 4 -and -not $connected; $try++) {
    $r = Adb connect $target
    Say "  adb: $r" DarkGray
    if ($r -match 'connected to' -or $r -match 'already connected') {
        # состояние устройства
        $state = Adb -s $target get-state
        if ($state -match 'unauthorized' -or $r -match 'unauthorized') {
            Say ''
            Say 'Посмотрите на экран проектора: должно появиться окно "Разрешить отладку?"' Yellow
            Say 'Отметьте "Всегда разрешать" и нажмите ОК пультом.' Yellow
            [void](Read-Host 'Когда нажмёте ОК, нажмите Enter здесь')
            continue
        }
        if ($state -match 'offline') {
            Say 'Проектор в состоянии offline, переподключаю...' Yellow
            [void](Adb disconnect $target)
            Start-Sleep -Seconds 2
            continue
        }
        if ($state.Trim() -eq 'device') { $connected = $true; break }
        Start-Sleep -Seconds 2   # состояние пока неясно, повторяем
    } else {
        if ($try -lt 4) { Start-Sleep -Seconds 2 }
    }
}
if (-not $connected) {
    $state = Adb -s $target get-state
    if ($state -match 'unauthorized') {
        Fail 'проектор не разрешил отладку. Примите окно "Разрешить отладку" на экране проектора и запустите снова.'
    }
    Fail "не удалось подключиться к $target. Проверьте IP, что компьютер и проектор в одной сети Wi-Fi и что отладка по сети включена."
}
Say "Подключено к $target." Green

# ---------- 5. install ----------
function Explain($out) {
    $map = [ordered]@{
        'INSTALL_FAILED_UPDATE_INCOMPATIBLE'      = 'на проекторе уже стоит приложение с таким же именем, но другой подписью. Удалите старое (Настройки -> Приложения -> Skreenshare проектор -> Удалить) и повторите.'
        'INSTALL_FAILED_VERSION_DOWNGRADE'        = 'на проекторе стоит более новая версия. Удалите её (Настройки -> Приложения) и повторите.'
        'INSTALL_FAILED_OLDER_SDK'                = 'Android проектора слишком старый для этого приложения (нужен Android 4.4 или новее). Если ставился обычный APK — положите рядом skreenshare-proektor-compat.apk и запустите снова.'
        'INSTALL_FAILED_INSUFFICIENT_STORAGE'     = 'на проекторе не хватает свободной памяти. Удалите ненужные приложения и повторите.'
        'INSTALL_FAILED_NO_MATCHING_ABIS'         = 'архитектура процессора не подходит (в приложении нет нативного кода, так быть не должно — сообщите об ошибке).'
        'INSTALL_FAILED_CPU_ABI_INCOMPATIBLE'     = 'архитектура процессора не подходит (сообщите об ошибке).'
        'INSTALL_FAILED_USER_RESTRICTED'          = 'установка запрещена настройками проектора. В "Для разработчиков" включите "Установка через USB/ADB" (если есть), на экране проектора подтвердите установку.'
        'INSTALL_FAILED_VERIFICATION_FAILURE'     = 'проверка пакета системой не прошла. Отключите "Проверять приложения при установке" в настройках Google Play/безопасности и повторите.'
        'INSTALL_FAILED_INVALID_APK'              = 'файл APK повреждён. Скачайте архив заново и распакуйте целиком.'
        'INSTALL_FAILED_ALREADY_EXISTS'           = 'приложение уже установлено (повторите — скрипт ставит с заменой).'
        'INSTALL_FAILED_DEXOPT'                   = 'система не смогла оптимизировать приложение (мало памяти или несовместимая прошивка).'
        'INSTALL_FAILED_TEST_ONLY'                = 'APK помечен как тестовый (сообщите об ошибке).'
        'INSTALL_PARSE_FAILED_NO_CERTIFICATES'    = 'прошивка проектора не приняла подпись APK. Попробуйте файл skreenshare-proektor-compat.apk и сообщите версию Android.'
        'INSTALL_PARSE_FAILED_MANIFEST_MALFORMED' = 'прошивка не смогла прочитать манифест APK. Попробуйте skreenshare-proektor-compat.apk.'
        'INSTALL_PARSE_FAILED_NOT_APK'            = 'файл не является APK (повреждён при копировании). Распакуйте zip заново.'
        'INSTALL_PARSE_FAILED_BAD_MANIFEST'       = 'прошивка не смогла прочитать манифест APK. Попробуйте skreenshare-proektor-compat.apk.'
        'INSTALL_FAILED_'                         = 'система проектора отклонила установку.'
        'INSTALL_PARSE_FAILED_'                   = 'система проектора не смогла разобрать APK.'
    }
    foreach ($k in $map.Keys) { if ($out -match [regex]::Escape($k)) { return $map[$k] } }
    return 'причина неизвестна, смотрите текст ошибки выше.'
}

function Install-Apk($file) {
    Say ''
    Say "Устанавливаю $(Split-Path -Leaf $file) ... (может занять до минуты)" Cyan
    $out = Adb -s $target install -r -d $file
    Say "  adb: $out" DarkGray
    return $out
}

$out = Install-Apk $apk
$ok = ($out -match '(?m)^\s*Success\s*$') -or ($out -match 'Success')
if (-not $ok -and $out -match 'INSTALL_FAILED_UPDATE_INCOMPATIBLE') {
    $a = (Read-Host 'Стоит приложение с другой подписью. Удалить его и поставить заново? (д/н)').Trim().ToLower()
    if ($a -in @('д', 'y', 'yes', 'да')) {
        Say (Adb -s $target uninstall $Package) DarkGray
        $out = Install-Apk $apk
        $ok = ($out -match 'Success')
    }
}

if ($ok) {
    Say ''
    Say 'ГОТОВО! Приложение "Skreenshare проектор" установлено.' Green
    $null = Adb -s $target shell am start -n "$Package/.MainActivity"
    Say 'Я запустил его на проекторе (если не открылось — найдите его в списке приложений).' Green
    Say 'Теперь на компьютере запустите skreenshare-proektor.exe и нажмите "Транслировать".' Green
    [void](Adb disconnect $target)
    exit 0
}

# ---------- failure ----------
$code = ''
if ($out -match '(INSTALL_[A-Z_]+)') { $code = $Matches[1] }
Say ''
Say 'Установить не удалось.' Red
if ($code) { Say "Код ошибки: $code" Red }
Say ("Что это значит: " + (Explain $out)) Yellow
Say ''
Say 'Если ошибку не удаётся исправить — пришлите этот текст целиком и версию Android проектора' Gray
Say '(Настройки -> О проекторе).' Gray
exit 1
