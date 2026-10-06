# ADR 008 — BOOTLOADER_RECLAIMABLE bölgelerine M2'de dokunulmuyor

Status: accepted (M2)

## Bağlam

Limine'in memory map'i, bu makinede 256 MiB ile boot edildiğinde dört ayrı
`bootloader-reclaimable` bölge bildiriyor, toplam ~4 MiB:

```
0x0000000000001000..0x0000000000052000    324 KiB  bootloader-reclaimable
0x000000000fbd5000..0x000000000fbdb000     24 KiB  bootloader-reclaimable
0x000000000fc03000..0x000000000ff34000   3268 KiB  bootloader-reclaimable
0x000000000ff51000..0x000000000ffdf000    568 KiB  bootloader-reclaimable
```

İsim davet edici: Limine işini bitirdi, bu bellek artık bizim. 251 MiB
kullanılabilir belleğin yanında %1,6 — küçük ama bedava.

Sorun şu ki **henüz işi bitmedi**. O bölgeler şunları barındırıyor:

- Limine'in response yapıları — kernel bunların her birini okuyor
- Memory map'in kendisi: `entries()` dizisi ve işaret ettiği `Entry`'ler
- Bootloader adı/sürümü gibi string'ler
- Boot sırasında kurulan sayfa tabloları — üzerinde çalıştığımız eşlemeler

Yani frame allocator'ı kurmak için yürüdüğümüz memory map, geri kazanmayı
düşündüğümüz belleğin içinde duruyor.

## Seçim

**Yalnızca `USABLE` allocator'a girer.** `is_allocatable()` tek bir tipi
kabul ediyor ve nedeni kodda yazılı.

Reclaimable bölgeler, bitmap'in "her şey dolu" başlangıcı sayesinde
otomatik olarak dışarıda kalıyor — ayrı bir dışlama listesi yok, dolayısıyla
unutulabilecek bir adım da yok.

Geri kazanım M3'e bırakıldı. Orada sırası şu olacak:

1. Hâlâ ihtiyaç duyulan her şeyi kopyala (memory map, HHDM offset, RSDP
   adresi, komut satırı).
2. Kendi sayfa tablolarımızı kur ve onlara geç — bootloader'ın tablolarına
   olan bağımlılığı kes.
3. Ancak o zaman reclaimable çerçeveleri serbest listeye ekle.

## Elenen alternatifler

**Şimdi geri kazanmak.** Çalışıyormuş gibi görünürdü: boot sırasında
okuduğumuz her şeyi zaten okumuş olurduk. Ama `MEMMAP.response()` ve
benzerleri `&'static` döndürüyor — Rust'ın bakış açısından o veri sonsuza
kadar geçerli, ve kernel'in herhangi bir yeri onu tekrar okuyabilir. Bir
ayırma o belleği üzerine yazdığında, hata ayırmanın yapıldığı yerde değil,
map'in bir sonraki okunduğu yerde ortaya çıkar.

**İhtiyacımız olanı kopyalayıp hemen geri kazanmak.** Doğru yaklaşım, ama
nereye kopyalayacağız? Statik bir tampon, önceden en büyük memory map'e
göre boyutlandırılmak zorunda. Heap M4'te. Frame allocator'dan ayırmak
tavuk-yumurta: map'i kopyalamak için allocator lazım, allocator'ı kurmak
için map. M3'te sayfa tabloları gelince bu düğüm çözülüyor.

**Yalnızca büyük bölgeyi (3268 KiB) geri kazanmak.** Hangi bölgenin neyi
barındırdığı protokolde tanımlı değil; Limine'in bir sonraki sürümünde
response'ları başka bir bölgeye koyması serbest. Tahmine dayalı bir
optimizasyon, karşılığı %1,3.

**ACPI_RECLAIMABLE'ı da almak.** Aynı sınıf hata, daha kötü sonuç: ACPI
tabloları henüz hiç okunmadı. M6/M7'de cihaz keşfi için gerekecekler.

## Sonuç

- Allocator 64 317 çerçeve (251 MiB) yönetiyor; ~1000 çerçeve (4 MiB)
  reclaimable olarak dokunulmadan duruyor.
- Limine'in response yapıları boot boyunca geçerli kalıyor; kernel onları
  ne zaman isterse okuyabilir.
- Kaybedilen %1,6, M3'te bir kerelik bir işle geri alınacak.
- Geri kazanımın ön koşulu kodda ve burada yazılı, böylece M3'te "neden
  yapmamıştık" sorusu tekrar sorulmayacak.
- Aynı gerekçe `ACPI_RECLAIMABLE` için de geçerli ve orası daha uzun süre
  dokunulmaz kalacak: ACPI tabloları M6/M7'ye kadar hiç okunmadı bile.