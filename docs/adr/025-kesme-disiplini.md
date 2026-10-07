# ADR 025 — IF disiplini: kesmeler ilk kez açılıyor

Status: accepted (M6)

M2'den M5'e kadar kernel'in her satırı tek bir varsayımın altında
yazıldı: **kesmeler kapalı.** Limine IF temiz girer, hiçbir yerde `sti`
yoktur, ve bu üç ayrı soundness argümanının ayağıdır — çerçeve
ayırıcının `unsafe impl Sync`'i, heap'in aynısı, ve capability
işlemlerinin bölünmezliği.

M6 bu varsayımı kaldırıyor. Bu ADR, yerine ne koyduğumuzu yazıyor.

## Neyin değiştiği

```
M5'e kadar:  IF = 0 her zaman. Varsayım evrensel, dolayısıyla görünmez.
M6'dan sonra: IF = 1 çoğu zaman. Varsayım yerel, dolayısıyla yazılmalı.
```

Görünmez bir varsayımın yerel hale gelmesi, onu **adlandırma**
zorunluluğu getiriyor. Aşağıdaki liste, "burada kesme olmamalı"
denilen her yeri sayıyor; listede olmayan her yer kesmeye açıktır ve
açık olduğu varsayılarak yazılmalıdır.

## Kesmeye kapalı bölgeler

### 1. Bağlam değiştirmenin kendisi

```
switch_context(from: *mut Context, to: *const Context)
```

RSP yarı yazılmışken gelen bir kesme, CPU'nun çerçeveyi nereye iteceğini
belirsiz bırakır. Daha incesi: `from`'un register'ları kaydedilmiş ama
`to`'nunkiler yüklenmemişken gelen bir kesme, zamanlayıcıyı iki
thread'in de "çalışıyor" göründüğü bir durumda yakalar.

Kapalı tutma biçimi: `switch_context` çağrılmadan önce `cli`, döndükten
sonra eski IF geri yüklenir. Fonksiyonun kendisi IF'e dokunmaz — bu,
çağıranın sorumluluğu, ve `debug_assert` ile doğrulanıyor.

**Neden `cli`/`sti` çifti değil de eski değerin geri yüklenmesi:** iç içe
çağrılarda `sti`, dıştaki bölgenin korumasını kaldırır. Kaydet-geri yükle
bu hatayı imkânsız kılar.

### 2. `retype`'ın ikinci fazı

ADR 020 retype'ı iki faza ayırdı: faz 1 doğrular ve hiçbir şeye
dokunmaz, faz 2 yazar ve başarısız olamaz. Faz 2'nin ortasında gelen bir
kesme tek başına zararsız — ta ki o kesme bir thread değişimine yol
açana kadar. O zaman başka bir thread, nesneler sıfırlanmış ama
capability'leri yazılmamış bir Untyped görür.

M6'da timer kesmesi **tam olarak bunu yapıyor.**

İlk taslak "faz 2 boyunca `cli`, faz 1 açık kalabilir" diyordu, çünkü
faz 1 hiçbir şey yazmıyor. Bu yanlış, ve yanlışlığının sebebi kaydedmeye
değer:

**Faz ayrımı bir atomiklik argümanıdır, bir eşzamanlılık argümanı
değil.** Faz 1'in işi, faz 2'nin başarısız olamayacağını *kanıtlamak* —
watermark yeter mi, hedef slot'lar boş mu, derinlik aşılıyor mu. Faz 2
bu kanıta güvenerek hiçbir kontrol yapmıyor.

Araya giren bir thread watermark'ı ilerletirse veya bir hedef slot'u
doldurursa, kanıt geçersizleşir ama faz 2 bunu fark etmez. Yani
koruma faz 2'ye değil, **kanıtın ömrüne** gerekiyor — ve o ömür faz
1'in başında başlıyor.

**Karar: retype'ın tamamı kesmeye kapalı.**

Maliyeti ölçülebilir ve küçük: 256 nesnelik bir retype'ın faz 2'si bir
`memset` ve 256 slot yazımı. `cap-revoke-depth` ölçümüne göre slot
başına ~2400 çevrim (emülasyonda), yani en kötü ~0.6 M çevrim — 10 ms
kuantumun %2'si. Gerçek donanımda çok daha az.

### 3. Derivation listesini değiştiren her şey

`copy`, `mint`, `delete`, `revoke`, `attach_child`. Liste çift bağlı ve
güncellemesi birden fazla yazma gerektiriyor; araya giren bir thread
yarı bağlanmış bir listeyi yürüyebilir.

**revoke bir istisna, ve kasıtlı.** ADR 019 "revoke kesmelere açık,
thread değişimine kapalı" diyor ve bu doğru kalıyor: revoke uzun sürer
(ölçüm: 8.4M slot için ~6.8 saniye), o süre boyunca cihaz kesmelerini
kaybetmek kabul edilemez. Korunması gereken şey listenin tutarlılığı,
ve onu bozan **thread değişimi**, kesmenin kendisi değil.

Yani iki ayrı kavram gerekiyor:

| | Kesme alınır mı | Thread değişir mi |
|---|---|---|
| `cli` bölgesi | hayır | hayır |
| "yield yasak" bölgesi | **evet** | hayır |

İkincisi için bir sayaç: `sched::PREEMPT_DISABLE`. Timer kesmesi gelir,
tick sayılır, ama sayaç sıfır değilse `yield` çağrılmaz — tick
ertelenir. Revoke bu bölgede çalışır.

### 4. Zamanlayıcının kuyruk işlemleri

Kuyruğa ekleme/çıkarma, `current` değişkeninin güncellenmesi. Kısa ve
`cli` ile korunuyor.

## Kesmeye açık olan ve öyle kalması gereken

- **Normal kernel akışı.** Boot'tan sonra IF set.
- **revoke'un tarama döngüsü** (yukarıdaki istisna).
- **Seri porta yazma.** Uzun sürer (115200 baud'da karakter başına ~87
  µs) ve kesme kapalı tutmak için hiçbir sebep yok. M6'nın testleri çok
  yazıyor; `cli` ile sarmak tick'leri kaybettirirdi.

  **Ama:** kesme handler'ı da seri porta yazıyor (panik raporu). İki
  yazar araya girerse çıktı karışır ve `run.py`'nin satır eşlemeleri
  bozulur. M6'da bu sorun yok çünkü timer handler'ı sessiz; ilk gürültülü
  handler geldiğinde seri port bir kilide ihtiyaç duyacak. **Bilinen
  eksiklere yazıldı.**

## Doğrulama: IF'in durumu gözlemleniyor

Yukarıdaki liste bir iddia, ve iddialar test edilmeli. `thread-test`
her korunan bölgenin içinde RFLAGS.IF'i okuyup basıyor:

```
[thread] IF discipline: ordinary kernel code runs with IF=1
[thread] IF discipline: inside an InterruptGuard, IF=0
[thread] IF discipline: nested guards do not re-open the outer region
[thread] IF discipline: inside a PreemptGuard, IF=1 and preemption is off
[thread] IF discipline: revoke ran and left IF=1
```

Sonuncusu özellikle önemli: revoke'un kesmeye *açık* olduğunu gösteriyor,
ki bu ADR 019'un kararı. Yanlışlıkla `cli` eklenirse bu satır değişir.

## Kaydedilen ikinci hata: bölgeden *çıkmadan* çıkmak

Yukarıdaki liste bölgeleri **girişlerine** göre saydı: "şurada `cli`,
burada geri yükle". Bu sayım bir durumu kaçırdı, ve kaçırdığı şey M6'yı
30 saniye astı.

Her bağlam değişimi kesmeye kapalı bir bölgenin içinde olur. Bölgeyi
kapatan `sti`, `switch_context`'in **döndüğü satırdadır**:

```rust
let guard = InterruptGuard::new();   // cli
switch_to(next);                     // buradan sonrası başka bir zaman
drop(guard);                         // sti
```

Daha önce çalışmış bir thread için bu doğrudur: onun kayıtlı RIP'i tam
olarak o `drop`'un öncesini gösterir, geri döndüğünde `sti`'yi çalıştırır.

**Hiç çalışmamış bir thread için yanlıştır.** Onun kayıtlı RIP'i kendi
entry noktasıdır; `switch_context`'in `ret`'i oraya doğrudan iner ve
yoldaki hiçbir `sti`'ye uğramaz. Thread, ömrü boyunca IF=0 ile çalışır.

Sonucu, hatanın kendisinden daha kötü: **yield etmeyen böyle bir thread
CPU'yu kalıcı olarak ele geçirir.** Onu almakla görevli timer, o
thread'in kendi RFLAGS'ında maskelidir. Zamanlayıcı kusursuz çalışıyordu;
kesme hiç gelmiyordu.

Kooperatif test bunu göremedi, çünkü orada her thread CPU'yu kendi
iradesiyle geri veriyor — ve `yield_now`'a yaptığı çağrı onu bölgenin
içine, dolayısıyla `sti`'nin önüne getiriyor. Hatayı yalnızca *hiç yield
etmeyen* bir thread ortaya çıkarabilirdi, ki `check_preemption` tam
olarak odur. Test, yazılma amacını doğruladı.

**Çözüm:** yeni bir thread entry noktasında başlamaz, bir trampoline'de
başlar:

```
thread_entry_trampoline:
    sti
    jmp r12
```

Gerçek entry noktası R12'de taşınır — callee-saved olduğu için bağlamın
olağan parçası olarak yüklenir, `switch_context`'e özel bir durum
eklemek gerekmez.

**Genel kural, listeye eklenen:** kesmeye kapalı bir bölgeyi `cli` ile
açıp karşı tarafta geri yüklemek, yalnızca bölgeden *aynı yoldan* çıkan
akışlar için yeterlidir. Bir bağlam değişimi bu varsayımı bozar: giren
thread ile çıkan thread aynı değildir. Bölgenin her **çıkışının** —
girişinin değil — IF'i geri yüklediği ayrıca gösterilmelidir.

### Bu hatayı nasıl test ediyoruz

İlk refleks, `check_preemption`'ın içine "thread IF=1 ile mi başladı"
kontrolü koymaktı. Mutasyon testi bunun işe yaramadığını gösterdi:
trampoline kaldırılıp koşulduğunda kontrol **hiç çalışmadı**. Çalışması
için `yield_now`'un dönmesi gerekiyor, ve hatanın tanımı tam olarak
dönmemesi. Test yalnızca hata yokken doğru cevabı veriyordu.

Bir hatanın sonucu "gözlemleyecek CPU kalmaması" olduğunda, sonucu
gözlemleyen bir kontrol yazılamaz. Değişmez, CPU teslim edilmeden
**önce** doğrulanmalı — thread henüz durmuşken, kayıtlı RIP'ine
bakarak:

```rust
let configured = tcb::read_registers(tcb_slot)?;
assert_eq!(configured.rip, sched::entry_trampoline());
```

Mutasyonla ölçülen fark: 30 saniyelik sessiz zaman aşımı → 0.9
saniyede, sebebini ve bu ADR'yi adıyla söyleyen bir panic.

**Kural:** bir hata sistemi ilerlemekten alıkoyuyorsa, onu *sonradan*
yakalayan test yazılamaz. Böyle değişmezler, etkileri ortaya çıkmadan
önce statik olarak kontrol edilmeli.

## interrupt_depth artık gerçek

ADR 022'nin tripwire'ı M5'te yapay bir bağlamda test edildi — sayacı
elle artırıp allocate çağırdık, çünkü gerçek kesme yoktu. M6'da gerçek
timer kesmesi var ve `thread-interrupt-alloc` senaryosu tripwire'ı
**gerçek bir kesme handler'ının içinden** tetikliyor.

Dispatcher artık her kesmede `enter_interrupt()` / `exit_interrupt()`
çağırıyor. Bu, M5'te yazılmış ama hiç çalışmamış bir yol.

## Elenen alternatifler

**Kesmeleri hiç açmamak, timer'ı polling ile okumak.** Preemption'ı
taklit edebilirdik: her yield'de TSC'ye bakıp kuantum dolmuşsa geç.
Ama bu kooperatif kalırdı — sonsuz döngüdeki bir thread'i kesemezdi, ki
preemption'ın tanımı o. Ve M12'de cihaz kesmeleri zorunlu; riski
ertelemek azaltmaz.

**Trap gate kullanmak (IF'i kapatmayan).** İç içe kesmelere izin verir,
tek seri port ve kilitsiz bir kernel'de çıktıyı karıştırır. Interrupt
gate'te kalıyoruz (ADR'siz, idt.rs'in yorumunda zaten yazılı).

**Her şeyi `cli` ile sarmak.** Basit ve yanlış: revoke'un 6.8 saniyesi
boyunca cihaz kesmesi kaybetmek, ADR 019'un açıkça reddettiği şey.

## Sonuç

- M2'den beri geçerli olan "kesmeler kapalı" varsayımı **kalktı**;
  yerine adlandırılmış bölgeler geldi.
- `cli` ile korunan: bağlam değiştirme, **retype'ın tamamı**, `copy`,
  `mint`, `delete`, zamanlayıcı kuyruğu.
- Kaydedilen hata (yazarken): "faz 2 yeter" ilk cevaptı ve yanlıştı.
  Faz ayrımı atomiklik içindir; korunması gereken şey faz 1'in ürettiği
  **kanıtın ömrü**, ve o ömür faz 1'in başında başlar.
- Kaydedilen hata (çalıştırırken): bölgeler girişlerine göre sayıldı,
  ama bir bağlam değişimi bölgeye bir thread'le girip başkasıyla çıkar.
  Hiç çalışmamış bir thread `sti`'yi atlayıp IF=0 ile başlıyordu ve
  yield etmediğinde CPU'yu kalıcı olarak alıyordu. Çözüm: `sti` yapan
  bir giriş trampoline'i. **Her çıkış IF'i geri yüklemeli, her giriş
  değil.**
- Ayrı bir kavram: **preempt-disable**, kesmeye açık ama thread
  değişimine kapalı. revoke burada çalışıyor (ADR 019).
- IF'in durumu her korunan bölgede seri porta basılıyor ve `run.py`
  denetliyor — iddia değil, gözlem.
- ADR 022'nin `interrupt_depth` tripwire'ı ilk kez gerçek bir kesme
  handler'ında test ediliyor.
- **Bilinen eksik:** seri port kilitsiz; gürültülü bir kesme handler'ı
  geldiğinde çıktı karışabilir.