# ADR 028 — Revoke edilmiş bir VSpace'e bağlı thread

Status: accepted (M7)

ADR 026 çalışan bir thread'in TCB'sini yok etmeyi reddetti. M7 aynı
soruyu bir seviye yukarıdan soruyor: bir thread'in **adres uzayı**
altından çekilirse ne olur?

Soru M6'dakinden farklı ve daha tehlikeli, çünkü burada söz konusu olan
nesne CPU'nun doğrudan yürüdüğü bir veri yapısı. Bir TCB revoke
edilirse zamanlayıcı bozulur; bir PML4 revoke edilirse **donanım** bozuk
bellekte sayfa tablosu yürür.

## İki ayrı durum, iki ayrı cevap

İlk taslak tek bir cevap aradı ve bulamadı. Sebebi, sorunun aslında iki
soru olması:

```
revoke(VSpace kökü)
  |
  +-- Bu kök ŞU AN CR3'te mi?        -> durum A
  +-- Başka bir thread'e mi bağlı?   -> durum B
```

### Durum A: çalışan thread'in VSpace'i — REDDEDİLİR

CR3 bu PML4'ü gösteriyor. Revoke, tabloların belleğini Untyped'a geri
veriyor; bir sonraki retype onu başka bir nesneye dönüştürüyor. Bu
andan itibaren CPU'nun her sayfa yürüyüşü, artık sayfa tablosu olmayan
bellekte yürüyor.

Sonucu bir hata değil, **tanımsız davranış**: rastgele bir fiziksel
adrese eşlenmiş rastgele izinler. Kesmeler bile kurtarmaz, çünkü IDT'ye
ulaşmak da aynı tablolardan geçiyor. Bulunabilecek en kötü hata türü —
geç, dolaylı ve tekrarlanamaz.

`CapError::VSpaceInUse` ile reddediliyor. Kontrol tek bir karşılaştırma:
kök adresi `sched::current_vspace()` ile aynı mı? Sınırsız döngü yok,
ADR 019'un kuralı korunuyor.

**Bu bir DoS değil**, ADR 026'nın argümanıyla aynı sebepten: çağıran
önce `SetSpace` ile kendini kernel VSpace'ine taşıyabilir, ya da
başka bir thread'den revoke edebilir. İki adımlı dizi her zaman
mümkün, dolayısıyla reddetme bir çıkmaz değil.

### Durum B: başka bir thread'in VSpace'i — İZİN VERİLİR, geçişte yakalanır

Thread çalışmıyor; CR3 onun köküne bakmıyor. Revoke güvenle tamamlanıyor.

Sorun bir sonraki `switch_to` denemesinde ortaya çıkıyor, ve orada
yakalanıyor: zamanlayıcı o thread'e geçmeyi **reddediyor**, onu
`Suspended` yapıyor ve kuyruktan çıkarıyor.

Neden revoke sırasında tüm TCB'leri dolaşıp onları durdurmuyoruz:
çünkü o bir tarama, ve taramanın uzunluğu sistemdeki thread sayısı.
ADR 019 revoke'u zaten 6787 ms ölçtü; üstüne bir tarama daha eklemek
yanlış yönde bir adım. Kullanım anında tespit, M5'in generation
sayaçlarıyla aynı felsefe: **eskimiş bir referansı kullanıldığında
yakala, üretildiğinde değil.**

## Eskimeyi nasıl tespit ediyoruz

TCB iki şey saklıyor:

```rust
vspace_root: u64,       // kök PML4'ün fiziksel adresi
vspace_generation: u8,  // SetSpace anındaki slot generation'ı
```

İkisi birden gerekli ve sebebi M5'te öğrenildi. Yalnızca adres yeterli
değil: revoke edilen bellek yeniden retype edilirse **aynı adreste**
yeni bir PageTable belirebilir — watermark sıfırlandığı için bu
olasılık yüksek, uzak değil. O zaman eski thread, kendisine ait olmayan
bir adres uzayında çalışmaya başlardı.

Generation bu takma adı yakalıyor. Tam olarak ADR 018'in CPtr için
çözdüğü sorun, farklı bir kılıkta: *"bir referans aynı nesneyi
gösterir ya da hata verir; asla başka bir nesneyi göstermez."*

Geçiş anındaki kontrol:

```rust
if slot.is_empty()
    || slot.cap_type() != CapType::PageTable
    || slot.generation() != tcb.vspace_generation
{
    // Eskimiş: thread'i durdur, geçme.
}
```

Üç karşılaştırma, döngü yok.

## Neden "sessizce kernel VSpace'ine düşmek" değil

Cazip bir alternatif: VSpace gitmişse thread'i kernel adres uzayında
çalıştırmaya devam et. Hiçbir şey durmaz, sistem akmaya devam eder.

Reddedildi, ve sebebi bu projenin tekrar eden dersi. Bir thread'in
adres uzayı, onun *görebildiği şeylerin tanımı*. Onu habersizce
değiştirmek, bileşeni başkasının belleğine bakarken bulmaktır — hem de
tam olarak kernel'in belleğine. Bir izolasyon mekanizmasının sessiz
geri çekilmesi, mekanizmanın hiç olmamasından kötüdür: ikincisinde en
azından kimse güvenmez.

Durma gürültülü. Thread `Suspended` oluyor ve seri porta sebebi
yazılıyor.

## Ne reddedilmedi: eşlemelerin kendisi

Revoke, VSpace'in *içindeki* eşlemeleri ayrıca temizlemiyor. Gerek yok:
PageTable'ların kendisi revoke ile Untyped'a dönüyor, yani ağacın
tamamı tek hamlede yok oluyor. Frame'ler ayrı capability'ler ve
sahiplerinde kalıyor — eşlenmiş olmaları onları VSpace'in parçası
yapmıyor.

Bu, ADR 020'nin watermark kararının doğrudan sonucu ve hoş bir tanesi:
bir adres uzayını yıkmak, nesne nesne dolaşmak değil, tek bir
watermark sıfırlaması.

## Elenen alternatifler

**Her iki durumda da reddetmek.** Tutarlı ve fazla katı. Durum B'de
gerçek bir tehlike yok — thread çalışmıyor — ve reddetmek, ölü bir
thread'in belleğini rehin tutardı.

**Her iki durumda da izin vermek.** Durum A tanımsız davranış. Yukarıda.

**Revoke sırasında bağlı thread'leri taramak.** Doğru sonucu verir,
sınırsız bir tarama ekler. ADR 019'un ölçümünden sonra revoke'a iş
eklemek savunulamaz.

**Thread'i öldürmek (suspend yerine).** ADR 026 zaten zombie
durumlarının M9'a ait olduğuna karar verdi. `Suspended` mevcut ve
yeterli: thread durmuş, durumu okunabilir, TCB'si revoke edilebilir.

## Sonuç

- **Durum A** (kök CR3'te): revoke `VSpaceInUse` ile reddedilir. Tek
  karşılaştırma. `SetSpace` her zaman mümkün olduğundan çıkmaz değil.
- **Durum B** (başka thread'e bağlı): revoke başarılı. Thread bir
  sonraki geçiş denemesinde `Suspended` yapılır ve sebebi basılır.
- Tespit **adres + generation** ile; yalnızca adres, yeniden
  kullanılmış bellekte takma ada açık kapı bırakırdı (ADR 018'in
  dersi).
- Sessizce kernel VSpace'ine düşmek reddedildi: bir izolasyon
  mekanizmasının sessiz geri çekilmesi, hiç olmamasından kötüdür.
- Hiçbir yol sınırsız döngü içermiyor.