# ADR 005 — Double fault'u IST1 üzerinde ayrı bir stack'te karşılamak

Status: accepted (M1)

## Bağlam

Bir exception alındığında CPU, handler'a geçmeden önce mevcut stack'e bir
frame (RIP, CS, RFLAGS, RSP, SS ve varsa hata kodu) iter. Bu push'un
başarısız olabileceği durumlar vardır:

- Stack taşmıştır ve RSP eşlenmemiş bir sayfayı gösterir.
- Stack pointer bozulmuştur (kayıt ezilmesi, yanlış hesaplanmış bir offset).
- Stack sayfası salt-okunur hale gelmiştir.

Bu durumda CPU, exception'ı teslim etmeye çalışırken ikinci bir exception
alır: tanımı gereği bir **double fault** (#DF). #DF handler'ı aynı bozuk
stack üzerinde çalışmaya kalkarsa, onun frame'ini itmek de başarısız olur —
bu üçüncü hata **triple fault**'tur ve mimari gereği makineyi sıfırlar.

Triple fault'un pratik sonucu şudur: QEMU yeniden başlar, seri port
bomboştur, hiçbir hata mesajı yoktur. Hata ayıklaması en pahalı olan hata
sınıfı budur.

## Seçim

TSS'in **Interrupt Stack Table**'ı (IST) kullanılıyor. IDT'nin #DF kapısına
IST index 1 yazıldığında CPU, frame'i itmeden **önce** RSP'yi koşulsuz olarak
`tss.ist[0]`'a yükler. Mevcut RSP ne olursa olsun — eşlenmemiş, hizasız,
sıfır — dikkate alınmaz.

```rust
// kernel/src/idt.rs
let ist = if vector == 8 { gdt::DOUBLE_FAULT_IST_INDEX } else { 0 };
```

IST1 için 16 KiB'lık, 16 bayt hizalı statik bir dizi ayrıldı. Handler
`qemu::exit` ile çıkar; `iretq` denemez, çünkü mimari #DF'den dönüşü
desteklemez.

### Testin kendini doğrulaması

`fault-df` senaryosu double fault'u **özellikle RSP'yi bozarak** üretir:

```rust
asm!("mov rsp, {bad}", "int3", bad = in(reg) PAGE_FAULT_ADDRESS, ...)
```

Bu seçim bilinçli. #DF'yi başka yollarla da üretebilirdik (örneğin vektör
14'ü not-present yapmak), ama o zaman #DF sağlam bir stack üzerinde teslim
edilirdi ve stack değişimi hakkında hiçbir şey kanıtlamazdı. Burada ise:

- IST çalışıyorsa → handler IST1'de koşar, rapor basılır, exit 35.
- IST çalışmıyorsa → triple fault, sessiz reset, seri port boş.

Handler ayrıca kendi RSP'sini okuyup IST1 aralığında olduğunu doğrular ve
`IST1: ok` basar. Runner hem bu satırı hem de "banner iki kez görünmemeli"
kuralını arar; ikincisi reset'i yakalar.

## Elenen alternatifler

**#DF'yi normal stack'te karşılamak.** Çalıştığı tek durum, #DF'nin
stack'ten *bağımsız* bir sebeple oluştuğu durumdur — ki bu, #DF'nin en sık
görülen nedeni değildir. Stack kaynaklı bir #DF'de doğrudan triple fault'a
gider.

**Her exception'ı IST üzerinde karşılamak.** 7 IST yuvası var, 32 vektör.
Daha önemlisi, iç içe geçmiş exception'lar aynı IST stack'ini yeniden
kullanır ve birbirinin frame'ini ezer. IST, "bu handler'ın stack'e
güvenemeyeceği" durumlar için nokta atışı bir araçtır, genel bir çözüm
değil.

**Stack'e guard page koymak.** Taşmayı #PF'ye çevirerek erken yakalar ve
iyi bir fikirdir — ama sayfa tablosu yönetimi gerektirir, o da M2/M3'ün işi.
Ayrıca guard page taşmayı yakalar, bozuk RSP'yi yakalamaz. İkisi tamamlayıcı;
IST olmadan guard page de triple fault'u engellemez.

**NMI'yı da IST'ye almak.** Doğru bir refleks (NMI herhangi bir anda, hatta
exception teslimi sırasında gelebilir), ama M1'de NMI üretecek bir kaynak
yok. IST2 boş duruyor; gerektiğinde eklenecek.

## Sonuç

- Bozuk bir stack pointer artık teşhis edilebilir bir rapora yol açıyor,
  sessiz bir reset'e değil.
- Kabul testi mekanizmayı iki yönlü doğruluyor: `IST1: ok` satırı pozitif
  kanıt, "banner iki kez görünmemeli" kuralı da negatif kanıt.
- TSS'in `rsp0` alanı ayrıldı ama boş; ring 3 geldiğinde (M8) dolacak.
  Yapıyı şimdi doğru kurmanın bedeli birkaç satır, sonradan eklemenin bedeli
  TSS'i yeniden düşünmek olurdu.
- 16 KiB statik olarak ayrılmış durumda. Kernel boot sonrası hiç allocate
  etmediği için bu bellek image'ın bir parçası; `.bss`'te duruyor.
- M2'de sayfa tabloları geldiğinde IST1 stack'inin altına bir guard page
  koymak doğal bir iyileştirme olacak.