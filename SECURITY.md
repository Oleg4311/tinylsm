# Политика безопасности

## Как сообщить об уязвимости

Пожалуйста, **не создавайте публичный issue**. Сообщите приватно через GitHub: вкладка **Security** → **Report a vulnerability**
([создать сообщение](https://github.com/Oleg4311/tinylsm/security/advisories/new)).

Укажите, что именно не так, как это воспроизвести и какая версия затронута. Я постараюсь ответить в течение недели, но это проект одного человека, поэтому сроки не гарантирую.

## Поддерживаемые версии

Исправления выходят для последней версии: ветка `main` и последний релиз.

## Что считается уязвимостью

tinylsm читает файлы с диска, которые мог подменить или испортить кто-то другой, поэтому интересны: паника, зависание или неограниченный расход памяти на вредоносных или повреждённых файлах (журнал, манифест, SSTable, `CURRENT`), приём повреждённых данных за корректные (обход контрольных сумм), выход за пределы каталога базы через имена в `CURRENT`, потеря подтверждённых записей при штатных сбоях, гонки при совместном использовании `Db` из нескольких потоков, небезопасное создание временных файлов.

## Что вне области

Сжатие и шифрование данных (их нет, это описано в README), защита от пользователя с правом записи в каталог базы (он может просто удалить данные), отказы оборудования, которое врёт про fsync, известные проблемы в зависимостях разработки (их закрывает Dependabot), отчёты без способа воспроизведения.

---

## English

Please report vulnerabilities privately via GitHub Security Advisories (**Security** → **Report a vulnerability**) and do not open a public issue. Include what is wrong, how to reproduce it and which version is affected. This is a one-person project, so response times are best-effort. Only the latest release and `main` receive fixes. In scope: panics, hangs or unbounded memory use on hostile or corrupted files (log, manifest, SSTables, `CURRENT`), corrupted data accepted as valid, path escape through names in `CURRENT`, loss of acknowledged writes under ordinary crashes, data races when a `Db` is shared between threads, unsafe temporary files. Out of scope: compression and encryption (not provided), attackers who can already write to the database directory, hardware that lies about fsync, known dev-dependency issues.
