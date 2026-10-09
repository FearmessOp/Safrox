# ADR 051 — PAT ve write-combining

- **Durum:** Kabul edildi
- **Tarih:** M14
- **Bağlam:** ADR 009 (adres uzayı), ADR 011 (sayfa tablosu API),
  ADR 017 (device Frame'leri NO_CACHE ile eşlenecek)
- **Referans:** Intel SDM Vol. 3A, 12.5 "Cache Control" ve 12.12
  "Page Attribute Table"; Tablo 12-10 (PAT reset değerleri),
  Tablo 12-11 (PAT girdisi seçimi)

## Bağlam

Framebuffer'a yazmak, belleğe yazmak gibi görünür ve değildir. Yazılan
şey bir aygıtın belleği ve yol PCIe üzerinden gider.

Üç önbellek tipi söz konusu:

- **WB (write-back)**: yazma önbelleğe gider, sonra tahliye edilir.
  Framebuffer için **yanlış**: çizim doğru görünür ama ne zaman
  görüneceği belirsiz, ve bir `clflush` gerekir.
- **UC (uncacheable)**: her yazma tek tek, sırayla, bus'a gider.
  Doğru ama **yavaş** — 4 baytlık bir piksel yazması bir PCIe
  işlemi.
- **WC (write-combining)**: yazmalar bir buffer'da toplanır ve 64
  baytlık burst'ler hâlinde gönderilir. Framebuffer'ın doğru tipi:
  sıralama garantisi yok, ama bir çerçeve tamponu için sıralama
  zaten gerekmiyor.

## Olgu: PAT hiç yapılandırılmamış

`cpu.rs` EFER, STAR, LSTAR ve FMASK yazıyor. **IA32_PAT (MSR 0x277)
hiç yazılmıyor.**

Reset değerleri (SDM Tablo 12-10):

| Girdi | PAT | PCD | PWT | Tip |
|---|---|---|---|---|
| PA0 | 0 | 0 | 0 | WB |
| PA1 | 0 | 0 | 1 | WT |
| PA2 | 0 | 1 | 0 | UC- |
| PA3 | 0 | 1 | 1 | UC |
| PA4 | 1 | 0 | 0 | WB |
| PA5 | 1 | 0 | 1 | WT |
| PA6 | 1 | 1 | 0 | UC- |
| PA7 | 1 | 1 | 1 | UC |

**Varsayılanda WC yok.** PA4-PA7, PA0-PA3'ün tekrarı. Dolayısıyla
write-combining istiyorsak PAT'ı yazmak bir tercih değil, zorunluluk.

## Karar: PA4 = WC, PA0-PA3 dokunulmaz

Yazılan değer yalnızca PA4'ü değiştiriyor:

```
PA7..PA0 = UC, UC-, WT, WB, UC, UC-, WT, [WC]
                                          ^^^^ PA4, 0x06 yerine 0x01
```

**PA0-PA3 neden dokunulmaz.** Çekirdeğin mevcut her eşlemesi ve
Limine'in bıraktığı her eşleme bu dört girdinin anlamına göre
çalışıyor: PMAP penceresi WB, LAPIC ve I/O APIC `NO_CACHE` ile yani
PA2/PA3. Bunlardan birini değiştirmek, hiçbir satırı değiştirmeden
mevcut eşlemelerin anlamını değiştirmek olurdu — ve belirtisi,
değiştiren satırla hiçbir görünür ilişkisi olmayan bir yerde ortaya
çıkardı.

PA4'ü seçmek, eski eşlemelerin hepsini olduğu yerde bırakıyor:
PAT biti sıfır olan her eşleme hâlâ eskisi gibi çözülüyor.

## Tehlike: bit 7 iki şey birden

**Bu ADR'nin asıl bulgusu**, ve kodu okurken çıktı.

4 KiB'lik bir PTE'de **bit 7 = PAT**. Bir PDE veya PDPTE'de **bit 7
= PS** (page size). Aynı bit, seviyeye göre farklı anlam.

`paging.rs`'te bu bit tek bir isimle tanımlı:

```rust
/// Bit 7, PS: this entry maps a large page rather than a table.
const HUGE = 1 << 7;
```

WC için leaf PTE'de bit 7'yi set etmek gerekiyor. Yani **mevcut kodun
`HUGE` diye okuduğu biti, leaf seviyesinde başka bir amaçla set
edeceğiz.**

Bugünkü kodun bundan zarar görmediğini doğruladım, ve doğrulamanın
sonucu rahatlatıcı olmaktan çok uyarıcı:

| Çağrı | Seviye | Durum |
|---|---|---|
| `walk_create` (498) | 4..2 | Yalnızca ara seviyeler. Güvenli. |
| `map_table` (913) | ≥ level+2 | Ara seviyeler. Güvenli. |
| `remove_table` (975, 990) | ≥ level+1 | Ara seviyeler. Güvenli. |
| `walk_lookup` (557) | 1..4 | `level == 1 \|\| entry.is_huge()` |

Son satır önemli: `walk_lookup` leaf'e de bakıyor, ama `||`'ın kısa
devresi sayesinde `level == 1` olduğunda `is_huge()` **hiç
çağrılmıyor**. Yani kod doğru çalışıyor — bir ifadenin değerlendirme
sırası sayesinde.

Bu, kasıtlı bir koruma değil; tesadüfi bir koruma. Ve tesadüfi
korumalar yeniden yazılırken kaybolur: biri o satırı
`entry.is_huge() || level == 1` diye değiştirse, WC eşlemeleri
sessizce "huge page" olarak raporlanmaya başlardı.

**Alınan önlem:** `EntryFlags`'e ayrı bir `PAT` sabiti ekleniyor, aynı
biti gösteren ikinci bir isim olarak, ve ikisinin de yorumun seviyeye
bağlı olduğunu söyleyen bir not taşıması sağlanıyor. `is_huge()`
artık seviyeyi parametre alıyor ve leaf'te daima `false` dönüyor —
kısa devreye güvenmek yerine, kuralı söyleyen bir imza.

İsim çakışmasını çözmemek, "bugün çalışıyor" ile "doğru" arasındaki
farkı kapatmış olurdu.

## Ölçüm: yapılacak, ama gerekçe değil

M14'ün kabul testi tam ekran doldurmayı WC'li ve WC'siz ölçüp iki
rakamı da basıyor.

**İki rakam arasındaki fark, WC'nin gerekçesi değildir.** QEMU'nun
TCG'si write-combining buffer'ı modellemiyor: WC'nin asıl kazancı 64
baytlık burst toplama, ve emülasyonda her yazma zaten bir fonksiyon
çağrısı. Fark çıkmazsa bu, PAT'a gerek olmadığını değil, **ölçüm
aracının bunu gösteremediğini** kanıtlar.

Bu ayrımı yapmak gerekiyor çünkü aynı hata iki kez yapıldı: M5'in
revoke maliyeti tahmini ve M8'in ReplyRecv tasarrufu, ikisi de
emülasyon altında ölçülüp gerçek donanımda farklı çıkacak şeylerdi.
Buradaki fark, bu sefer **önceden** söylenmiş olması.

Ölçüm yine de yapılıyor, iki sebeple: WC'li yolun *çalıştığını*
gösteriyor (eşleme kuruluyor, pikseller doğru), ve gerçek donanımda
koşulduğunda karşılaştırılacak bir taban veriyor.

`docs/hardware-todo.md`'ye madde eklendi.

## Elenen alternatifler

**(a) UC ile yetinmek.** ADR 017 device Frame'leri için `NO_CACHE`
demişti ve bu LAPIC/IOAPIC için doğru — oradaki her erişim bir
register ve sıralama önemli. Framebuffer farklı: milyonlarca
baytlık bir veri yüzeyi, sıralaması önemsiz. Aynı bayrağı ikisine de
uygulamak, "device" kelimesinin iki farklı şeyi adlandırdığını
görmezden gelmek olurdu.

**(b) MTRR kullanmak.** PAT'tan eski mekanizma, fiziksel aralıklara
tip atar. Daha az esnek (sabit sayıda aralık, hizalama kısıtları) ve
PAT'la etkileşimi kendi başına bir tablo. PAT, sayfa başına karar
veriyor ve zaten sayfa tablosu yazıyoruz.

**(c) WC'yi PA0-PA3'ten birine koymak.** Yukarıda: mevcut
eşlemelerin anlamını sessizce değiştirirdi.

## Sonuçlar

- `IA32_PAT` boot'ta bir kez yazılıyor, PA4 = WC, geri kalanı reset
  değerinde.
- `PageFlags`'e `WRITE_COMBINING` eklendi; leaf entry'de PAT bitini
  set ediyor, PWT ve PCD'yi temiz bırakıyor.
- `is_huge()` artık seviyeyi alıyor. Bit 7'nin iki anlamı kodda
  ayrı ayrı adlandırılmış durumda.
- WC yalnızca framebuffer için kullanılıyor. LAPIC, I/O APIC ve
  gelecekteki register pencereleri `NO_CACHE` ile kalıyor.
- Ölçüm raporlanıyor, şerhiyle birlikte; gerçek donanım doğrulaması
  `hardware-todo.md`'de.