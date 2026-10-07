# ADR 014 — CPU özellik bitleri: NXE, SMEP, SMAP, PGE

Status: accepted (M3 implementation)

İlgili: [ADR 010](010-hhdm-kaderi.md) (W^X bu bitlere dayanıyor),
[ADR 012](012-tlb-gecersizlestirme.md) (PGE)

## Bağlam

M3'ün koruma iddialarının tamamı dört kontrol bitine bağlı, ve mimaride
bunların hiçbiri "zaten açıktır" diye varsayılabilecek durumda değil.
Bootloader bazılarını açmış olabilir — Limine büyük olasılıkla NXE'yi
açıyor — ama "büyük olasılıkla" bir koruma mekanizmasının temeli olamaz.

Dört bit, dört farklı aciliyet:

| Bit | Yer | M3'teki rolü | Yoksa |
|---|---|---|---|
| NXE | `IA32_EFER[11]` | W^X'in tamamı | Her NX eşlemesi ilk kullanımda #PF |
| SMEP | `CR4[20]` | User sayfasından komut getirmeyi engeller | M8'de daha zayıf izolasyon |
| SMAP | `CR4[21]` | User sayfasını okuma/yazmayı engeller | M8'de daha zayıf izolasyon |
| PGE | `CR4[7]` | Global bit anlam kazanır | CR3 her yazıldığında kernel eşlemeleri de atılır |

## NXE özel bir durum

Diğer üçü yoksa koruma zayıflar. NXE yoksa **hiçbir şey çalışmaz**, ve
sebebi ince:

Sayfa tablosu girdisinin 63. biti, `EFER.NXE` temizken "yürütülemez"
anlamına gelmez — *ayrılmıştır* (reserved). Ayrılmış bit set edilmiş bir
girdiden geçen her erişim #PF üretir, reserved-bit hata koduyla. Yani
NXE kapalıyken PMAP'i NX kurduğumuzu sandığımız an, PMAP'i tamamen
kullanılamaz kurmuş oluyoruz.

Başarısızlık gürültülü — bu iyi. Ama ilk erişimde ortaya çıkıyor, yani
sebepten uzakta — bu kötü. Bu yüzden NXE tek `assert!`li bit:

```rust
assert!(
    features.nx_enabled,
    "EFER.NXE could not be enabled (CPU support: {}); W^X is impossible \
     without it and every NX mapping would fault",
    nx_supported
);
```

Referans: Intel SDM Vol. 3A, 4.6 "Access Rights"; 4.9.2 ("execute-disable
... only if IA32_EFER.NXE = 1"); Vol. 4, Table 2-2 (MSR `0xC000_0080`).

## Seçim

**Sor, kur, geri oku, doğrula.** `cpu::init` dört adımı her bit için
uyguluyor:

1. **Sor.** CPUID ile destek kontrolü — `0x8000_0001` EDX[20] (NX),
   yaprak 7 alt-yaprak 0 EBX[7] (SMEP) ve EBX[20] (SMAP). Yaprak 7'nin
   kendisi var mı diye önce maksimum yaprak numarasına bakılıyor.
2. **Kur.** Desteklenen bitleri set et. CR4'te **yalnızca ekle**: mevcut
   değerden başlayıp OR'la, böylece PAE/PSE/OSFXSR gibi yönetmediğimiz
   bitler bozulmaz.
3. **Geri oku.** MSR'ı ve CR4'ü tekrar oku.
4. **Doğrula.** Okunan değerde bit yoksa, yazma tutmamıştır. NXE için bu
   panic; diğerleri için rapor.

Geri okuma adımı pahalı değil ve sessiz bir düşüşü yakalıyor: bir
hipervizör CR4 yazmasını kabul edip biti yok sayabilir, ve o durumda
"SMEP açtık" diye düşünmeye devam etmek, hiç denememekten kötüdür.

### SMEP/SMAP neden şimdi, user space yokken?

M3'te hiçbir user eşlemesi yok, dolayısıyla ikisi de hiçbir şeyi
korumuyor. Yine de şimdi açılıyorlar, çünkü **M8'de hatırlanması gereken
bir şey olmaktan çıkıyorlar.** Bir koruma mekanizmasını "sonra açarız"
listesine koymak, onu açılmamış bırakmanın en yaygın yolu.

Bedeli sıfıra yakın: iki CR4 biti ve yirmi satır CPUID kodu.

### QEMU'da desteklenmiyorlar

Test ortamı `-cpu qemu64` kullanıyor ve bu model SMEP/SMAP bildirmiyor.
Gözlenen çıktı:

```
[cpu] smep: unsupported, smap: unsupported, pge: enabled
[cpu] WARNING: SMEP/SMAP unavailable on this CPU; ring 3 in M8 will have
      weaker isolation
```

Kod bunu doğru ele alıyor — uyarır, devam eder — ama **korumanın kendisi
test edilmemiş durumda.** Bilinen eksiklere yazıldı. `-cpu host` ya da
`-cpu Skylake-Client` ile çalıştırmak bitleri getirir; M3'te test matrisini
genişletmek yerine eksik olarak kaydedildi, çünkü koruyacak bir user
sayfası olmadan testin kanıtlayacağı şey de sınırlı.

### SMAP ve kasıtlı user erişimi — M8 sanıldı, M7'de geldi

SMAP açıkken kernel'in bir user sayfasını okuması ya da yazması #PF üretir.
Bu istenen davranış, ama M8'de kernel'in user belleğine **kasıtlı** olarak
erişmesi gerekecek: syscall argümanlarını kopyalamak, bir tamponu
doğrulamak.

O erişimler `stac`/`clac` komut çifti arasına alınmak zorunda (EFLAGS.AC
biti geçici olarak set edilir). Unutulursa belirti, kernel'in kendi
`copy_from_user` eşdeğerinde #PF alması olur — gürültülü, ama ilk kez
karşılaşan için kafa karıştırıcı.

**M8'in bilinen eksiklerine yazıldı.** Ayrıca o noktada `stac`/`clac`'ı
elle yazmak yerine bir RAII sarmalayıcı (`UserAccess` guard) tercih
edilmeli: `Drop` ile `clac` garanti edilir ve erken `return` yüzünden
AC'nin açık kalması imkânsızlaşır.

#### M7 eki: borç bir milestone erken geldi

Yukarıdaki paragraf çözümü doğru tarif etmiş, zamanlamayı yanlış. "M8'de
kernel'in user belleğine erişmesi gerekecek" cümlesi, SMAP'in ne zaman
devreye girdiği hakkında sessiz bir varsayım taşıyor: ring 3 ile
birlikte.

**SMAP sayfanın USER bitine bakar, erişen kodun ayrıcalık seviyesine
değil.** M7 user adres uzayları kuruyor — yani USER işaretli sayfalar —
ama her thread hâlâ CPL 0'da çalışıyor. İlk USER sayfası ile ilk SMAP
ihlali aynı anda doğdu, ring 3'ten bir milestone önce.

`vspace-test`'in ilk koşusu bunu `supervisor read protection-violation`
olarak bildirdi: mevcut, okunabilir, doğru eşlenmiş bir sayfada. Hata
eşlemede değildi; eşleme kusursuzdu ve SMAP görevini yapıyordu.

`cpu::UserAccess` yazıldı, tam da yukarıda tarif edildiği gibi — RAII,
`Drop` ile `clac`, ve ADR 025'in `InterruptGuard` kuralıyla aynı
şekilde iç içe kullanıma güvenli (dıştaki bölgenin iznini iptal etmez).
Kabul testi üçünü de doğruluyor: AC sıradan kernel kodunda temiz,
guard içinde set, çıkışta yeniden temiz.

**Genel ders:** "X özelliği Y milestone'unda gerekecek" dendiğinde,
X'in neyi tetiklediği ile Y'nin ne getirdiğinin aynı şey olduğu
varsayılıyor olabilir. Burada değildi.

## Elenen alternatifler

**Bootloader'ın NXE'yi açtığını varsaymak.** Limine muhtemelen açıyor.
Ama protokol bunu garanti etmiyor, başka bir bootloader'a geçersek sessizce
bozulur, ve kontrolün maliyeti bir CPUID + bir RDMSR + bir WRMSR. Varsayım
ucuz değil, kontrol ucuz.

**Desteklenmeyen bir özellikte panic etmek (SMEP/SMAP için).** NXE için
doğru, bunlar için değil: M3'te korudukları bir şey yok, ve QEMU'nun
varsayılan CPU modelinde boot edemeyen bir kernel geliştirmeyi
zorlaştırır. M8'de bu karar yeniden ele alınmalı — o noktada "SMEP yoksa
ring 3'e geçme" savunulabilir bir pozisyon olur.

**Geri okumayı atlamak.** Yazdıktan sonra okumak gereksiz görünebilir;
donanım yazdığını tutar. Hipervizörler her zaman tutmaz, ve tutmadığında
sessizdir. İki `mov` karşılığında bir sınıf sessiz hata eleniyor.

**Özellikleri `Features` olarak döndürüp orada bırakmak.** M3'ün ilk
uygulaması böyleydi ve yığın değişimiyle çakıştı: `cpu::init`'in döndürdüğü
değer `kstart`'ın yığın çerçevesinde yaşıyordu, `kmain` ise başka bir
yığında çalışıyor. Çözüm, değeri bir `AtomicU64`'e paketleyip
`cpu::features()` ile tekrar okumak. Bir "geçerli" biti var, böylece
`cpu::init`'ten önce okuma "hiçbir şey desteklenmiyor" diye görünmek
yerine panic ediyor.

## Sonuç

- Dört bit CPUID'den sorulup set ediliyor, geri okunup doğrulanıyor.
- NXE zorunlu: olmazsa kernel duruyor. W^X'in tamamı ona bağlı ve eksikliği
  kuruluma uzak bir yerde belirti verirdi.
- SMEP/SMAP M3'te hiçbir şey korumuyor ama yine de açılıyor — M8'de
  hatırlanacak bir madde olmaktan çıksınlar diye.
- PGE açık; global bit [ADR 012](012-tlb-gecersizlestirme.md)'nin
  gerektirdiği anlamı kazanıyor.
- Durum serial'e basılıyor ve `vm-test` senaryosu NXE'yi ayrıca iddia
  ediyor; `vm-nx` senaryosu hata kodu bit 4 ile pratikte doğruluyor.
- `cpu::features()` yığın değişiminin ötesinde de okunabilsin diye
  statikten okuyor.
- **Bilinen eksik:** SMEP/SMAP `-cpu qemu64` altında desteklenmiyor, yani
  kod yolu çalışıyor ama koruma test edilmedi.
- ~~**M8'in borcu:**~~ **M7'de ödendi.** SMAP USER bitine bakar, CPL'e
  değil, dolayısıyla borç ring 3'ten bir milestone önce geldi.
  `cpu::UserAccess` guard'ı yazıldı ve `vspace-test` onu doğruluyor.