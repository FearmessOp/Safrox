# ADR 001 — Limine boot protocol, base revision 3

Status: accepted (M0)

## Bağlam

Kernel'in 64-bit long mode'da, sayfalama açık ve higher-half'ta başlaması
gerekiyor. Bu noktaya ulaşmanın iki yolu var: ya kendi boot kodumuzu yazıp
real mode'dan long mode'a geçişi, A20'yi, geçici GDT'yi ve ilk sayfa
tablolarını elle kurarız; ya da bunu yapan bir bootloader kullanırız.

M0'ın amacı boot mekaniği değil; amaç seri çıktı, panic raporlama ve
deterministik çıkış gibi test altyapısını kurmak. Ayrıca sonraki
milestone'larda bellek haritası, HHDM offset'i ve ACPI RSDP gibi bilgilere
ihtiyacımız olacak.

Limine crate'i (`BaseRevision`) varsayılan olarak kendi desteklediği en yüksek
revision'ı (şu an 6) ister. Bu, crate yükseltmeleriyle birlikte kernel'in
beklentilerinin sessizce kaymasına yol açar.

## Seçim

Limine boot protokolü, **base revision 3**'e sabitlenmiş olarak kullanılıyor:

```rust
static BASE_REVISION: BaseRevision = BaseRevision::with_revision(3);
```

Bootloader bu revision'ı karşılamazsa `BASE_REVISION.is_supported()` false
döner ve kernel boot'un en başında panic eder. Revision'ın sabitlenmesi,
`limine` crate'i yükseltildiğinde kernel'in hangi ABI'ye göre yazıldığının
değişmemesini sağlar; yükseltme bilinçli bir karar olur.

İstekler `.limine_requests` bölümünde, `#[used]` + `#[link_section]` ile
tutuluyor; linker script bunları `KEEP()` ile koruyor.

## Elenen alternatifler

**Kendi boot kodumuzu yazmak (real mode → long mode).** Birkaç yüz satır
assembly, hiçbiri bu projenin öğretmek istediği şeyle ilgili değil. Daha
kötüsü: hata ayıklaması zor, çünkü seri port henüz yokken kırılıyor. Sonraki
milestone'larda ihtiyacımız olan bellek haritası ve ACPI bilgilerini de
kendimiz toplamamız gerekirdi.

**Multiboot2 + GRUB.** Olgun ve yaygın, ama 32-bit protected mode'da bırakıyor
— long mode'a geçişi ve higher-half eşlemeyi yine biz yapacaktık. HHDM gibi
64-bit'e özgü kolaylıklar yok.

**Limine, revision sabitlenmeden (`BaseRevision::new()`).** Crate'in maksimumunu
ister. Çalışır, ama `cargo update` kernel'in konuştuğu protokolü değiştirebilir
ve bu değişiklik kod incelemesinde görünmez.

**UEFI uygulaması olarak doğrudan boot.** BIOS desteğini kaybederdik ve UEFI
boot services'ten çıkış (`ExitBootServices`) koreografisi M0'ın kapsamından
büyük.

## Sonuç

- Kernel 64-bit long mode'da, higher-half'ta, geçerli bir stack ile başlıyor.
- Bootloader adı/sürümü, HHDM offset'i ve executable adresi hazır geliyor;
  bunlar M0'ın kabul testinin doğruladığı satırlar.
- Protokol sürümü artık bir bağımlılık yükseltmesiyle sessizce değişemez.
- Bedeli: ISO'ya Limine blob'ları gömülüyor ve `third_party/limine` bir build
  bağımlılığı (ilk build'de otomatik çekiliyor).
- Revision 3'ten 6'ya geçiş ileride ayrı bir karar olacak; `is_supported()`
  kontrolü sayesinde uyumsuzluk sessiz kalmaz, boot'ta panic olur.