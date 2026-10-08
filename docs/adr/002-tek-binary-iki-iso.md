# ADR 002 — Tek kernel binary, iki ISO

Status: accepted (M0)

## Bağlam

M0'ın negatif testi, kernel'in panic yolunu doğrulamak zorunda: panic mesajı
seri porta dosya:satır ile düşmeli, kernel `0x11` yazmalı, QEMU 35 ile
kapanmalı.

Bir panic'i tetiklemenin en kolay yolu, kernel'i panic edecek şekilde
derlemek. Ama o zaman test edilen şey sevk edilen binary değil; panic yolunu
"test build"de doğrulayıp normal build'de bozuk bırakmak mümkün olur. Bu,
testin en çok işe yarayacağı durumu — gerçek bir hata anında panic'in
çalışması — kapsam dışı bırakır.

## Seçim

**Tek bir `kernel.elf`, iki ISO.** Her iki ISO da baytı baytına aynı kernel'i
içerir; fark yalnızca Limine yapılandırmasındadır.

`boot/limine.conf.in` bir şablondur ve iki menü girdisi tanımlar. Build
sırasında `@DEFAULT_ENTRY@` yerine `1` veya `2` konarak iki ayrı config
üretilir:

| ISO | `default_entry` | Girdi | Beklenen çıkış |
|---|---|---|---|
| `build/safrox.iso` | 1 | normal boot | 33 |
| `build/safrox-panic.iso` | 2 | `cmdline: panic-test` | 35 |

Kernel, Limine'den aldığı command line'da `panic-test` token'ını arar ve
bulursa kasıtlı olarak panic eder. Bu kontrol normal binary'nin içindedir;
`#[cfg(test)]` ya da feature flag arkasında değildir.

## Elenen alternatifler

**Ayrı bir panic build'i (`--features panic-test`).** Test edilen binary sevk
edilenden farklı olurdu. Panic yolu, onu gerçekten çalıştıracak koşullarda
hiç denenmemiş olurdu.

**Tek ISO, çalışma anında menü seçimi.** QEMU'ya tuş vuruşu göndererek
Limine menüsünden ikinci girdiyi seçmek mümkün. Zamanlamaya bağlı, kırılgan ve
seri logu menü gürültüsüyle kirletir — oysa runner tam da o logu denetliyor.

**QEMU `-append` / kernel cmdline'ı dışarıdan vermek.** Limine protokolünde
cmdline bootloader config'inden gelir; QEMU'nun `-append`'i yalnızca Linux
boot protokolünde anlamlıdır.

**Panic'i bir donanım hatasıyla tetiklemek (örneğin null dereference).** M0'da
henüz IDT yok; bir fault doğrudan triple fault'a ve reset'e giderdi, panic
handler hiç çalışmazdı. Bu test M1'de (IDT kurulduktan sonra) anlamlı olur.

## Sonuç

- Negatif test, sevk edilen binary üzerinde koşuyor.
- İki ISO'nun kernel'i aynı; `cmp` ile doğrulanabilir.
- `timeout: 0` sayesinde menü hiç görünmez, seri log temiz kalır.
- Bedeli: iki ISO üretiliyor (birkaç yüz KB). Build süresine etkisi ihmal
  edilebilir, çünkü kernel bir kez derleniyor.
- M1'de gerçek bir CPU exception'ı ile panic tetikleme eklendiğinde bu
  mekanizma yerinde kalır; cmdline hook'u o zaman da "panic altyapısı
  çalışıyor mu" sorusunun en hızlı cevabı olmayı sürdürür.