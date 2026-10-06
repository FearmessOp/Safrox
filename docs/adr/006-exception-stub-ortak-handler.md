# ADR 006 — Assembly stub'ları + tek ortak handler

Status: accepted (M1)

## Bağlam

IDT'deki her kapı bir adres gösterir ve CPU oraya atlar. Bu bir fonksiyon
çağrısı değildir; iki fark onu sıradan bir `extern "C"` fonksiyonuyla
uyumsuz kılar:

1. **Bazı vektörler hata kodu iter, bazıları itmez.** #DF, #TS, #NP, #SS,
   #GP, #PF, #AC, #CP, #VC ve #SX için CPU fazladan bir qword iter; diğer
   28 vektör için itmez. Handler'ın frame'i doğru yerden okuyabilmesi ve
   dönerken stack'i dengede bırakabilmesi için hangi durumda olduğunu
   bilmesi gerekir.

2. **Dönüş `iretq` ile olur, `ret` ile değil.** CPU'nun ittiği frame'i
   yalnızca `iretq` doğru tüketir.

Rust'ta `extern "C" fn` bunların hiçbirini yapmaz.

## Seçim

32 vektörün her biri için küçük bir assembly stub'ı, `global_asm!` içinde
`macro_rules!` ile üretiliyor. Stub'lar iki durumu tek bir şekle indirger:

```
isr_stub_N:             ; hata kodu itmeyen vektör
    push 0              ; sahte hata kodu
    push N
    jmp isr_common

isr_stub_M:             ; hata kodu iten vektör
    push M              ; CPU zaten hata kodunu itti
    jmp isr_common
```

`isr_common` 15 genel kaydı iter, `cld` yapar, `rdi`'ye frame adresini
koyar ve tek bir Rust fonksiyonunu — `exception_dispatch` — çağırır.
Dönüşte kayıtları geri alır, vektör+hata kodunu atar ve `iretq` yürütür.

Sahte `push 0` sayesinde dispatcher'ın bakış açısından 32 frame birbirinin
aynısıdır; `ExceptionFrame` tek bir `#[repr(C)]` struct ile tarif edilebilir.

### Sınıflandırma hatasına karşı derleme zamanı kontrolü

Hangi vektörün hata kodu ittiği bilgisi iki yerde duruyor: assembly'de hangi
makronun çağrıldığında, ve Rust'taki `pushes_error_code()` fonksiyonunda.
İkisi birbirinden ayrılırsa, etkilenen vektörün frame'i 8 bayt kayar ve
raporlanan RIP çöp olur — hex dökümünde fark edilmesi çok zor bir hata.

Bu yüzden `interrupts.rs` sonunda bir `const` blok ikisini karşılaştırıyor;
uyuşmazlık **derleme hatası** üretir, çalışma zamanı sürprizi değil.

## Elenen alternatifler

**`x86-interrupt` ABI (vektör başına bir Rust fonksiyonu).** En kısa yol;
derleyici hata kodunu ve `iretq`'yi kendisi halleder. İki sebeple elendi:

- Bu kernel'in açıkça göstermesi gereken mekanizmayı gizliyor. M1'in amacı
  "exception'lar çalışsın" değil, "exception'ların nasıl çalıştığı kodda
  görünsün".
- Kararsız bir derleyici özelliği (`feature(abi_x86_interrupt)`). Nightly'de
  olmamız `-Z build-std` için zorunlu, ama her kararsız özelliği benimsemek
  için gerekçe değil.

**Vektör başına ayrı Rust handler'ı yazmak.** 32 fonksiyon, 32 kez
tekrarlanan prolog/epilog. Hata kodu ayrımı yine elle yapılırdı ve her
kopyada ayrı ayrı yanlış yapılabilirdi.

**Tek bir stub ve vektörü CPU'dan okumak.** Böyle bir mekanizma yok; CPU
hangi vektörün işlendiğini handler'a söylemez. Vektör numarası ancak IDT'ye
yazdığımız adresten çıkarılabilir, yani vektör başına ayrı bir giriş
noktası zaten zorunlu.

**Hata kodunu dispatcher'da vektöre bakarak ayırmak (sahte push yok).**
Mümkün, ama frame'in yeri vektöre bağlı hale gelir: `ExceptionFrame` artık
tek bir struct olmaz, ya da her alan erişimi bir dallanma içerir. Sahte
`push 0` bu karmaşıklığı 28 stub'da birer bayta indiriyor.

## Sonuç

- Dispatcher tek bir yerde, tek bir frame şekliyle çalışıyor; yeni bir
  exception'ı özel ele almak `match frame.vector` içine bir kol eklemek
  demek.
- Hata kodu ayrımı iki yerde yazılı ama derleyici tarafından senkron
  tutuluyor.
- Tüm 32 vektör dolu. Beklenmeyen bir exception "bilinmeyen davranış"
  üretmiyor; raporlanıp 35 ile çıkıyor.
- 32..=255 arası vektörler not-present bırakıldı: oraya gelen bir kesme #GP
  üretir ve hata kodunda vektör numarası görünür — tanımsız belleğe
  atlamaktan çok daha bilgilendirici.
- `isr_common` 15 kaydın hepsini kaydediyor, yalnızca callee-saved olanları
  değil: exception herhangi bir komut sınırında gelebileceği için SysV'nin
  "caller-saved" kategorisi burada anlamsız.
- `cld`, SysV ABI'nin "C fonksiyonuna girişte DF temiz olmalı" kuralı için;
  kesilen kod DF'yi set bırakmış olabilir.
- Maliyeti: ~70 satır assembly ve bir makro çifti. Karşılığında exception
  giriş yolu okunabilir ve tek parça.