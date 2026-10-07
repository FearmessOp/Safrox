# ADR 024 — Zamanlayıcı kernel'de: politikanın geçici evi

Status: accepted (M6)

Master prompt'un kuralı açık: **kernel mekanizma sağlar, politika
sağlamaz.** Round-robin bir politikadır. Bu ADR, onu neden yine de
kernel'e koyduğumuzu ve ne zaman çıkaracağımızı yazıyor.

## Bağlam

M6'nın iki thread'i çalıştırması gerekiyor. "Çalıştırmak" iki ayrı şey:

- **Mekanizma:** bağlam değiştirme. Callee-saved register'ları kaydet,
  RSP'yi değiştir, geri yükle, dön. Hangi thread'e geçileceği bu
  sorunun parçası değil.
- **Politika:** *hangi* thread'e geçileceği. Round-robin mı, öncelikli
  mi, son kullanıma göre mi.

İkincisi kernel'de olmamalı. seL4 bunu şöyle çözer: kernel yalnızca
"bu thread'i çalıştır" mekanizmasını verir, karar bir user-space
scheduler bileşenindedir ve kernel ile IPC üzerinden konuşur.

## Neden şimdi yapamıyoruz

User-space scheduler'ın çalışması için üç şey gerekir ve üçü de yok:

1. **IPC.** Scheduler bileşeni kernel'den "thread bloke oldu" haberini
   almalı ve kernel'e "şimdi şunu çalıştır" demeli. IPC M9'da.
2. **User-space.** Scheduler bir bileşen, yani ring 3'te bir thread.
   Ring 3 M8'de.
3. **Scheduler'ı kim zamanlar?** Klasik özyineleme. Çözümü (seL4'ün
   scheduler context'leri, ya da Fiasco'nun yaklaşımı) kendi başına bir
   milestone.

Yani M6'da zamanlayıcı kernel'de olmak *zorunda*. Alternatif, M6'yı M9'a
ertelemek — ve M7-M8'i thread'siz yapmaya çalışmak, ki yapılamaz.

## Karar

**Round-robin, kernel'de, açıkça geçici olarak işaretlenmiş.**

```
sched::ready_queue : VecDeque<TcbRef>
sched::yield_now() -> sıradakine geç
sched::tick()      -> timer kesmesinde, kuantum dolduysa yield
```

Öncelik alanı TCB'de var ve `SetPriority` ile yazılıyor, ama M6'nın
zamanlayıcısı onu **kullanmıyor** — yalnızca saklıyor ve
`ReadRegisters` ile geri veriyor. Kullanılmayan bir alanı şimdi eklemek,
M10'da TCB'nin boyutunu değiştirmekten ucuz (ADR 017'nin tavanı 2048
bayt ve fazlasıyla yer var).

Bunu yazmak önemli: **M6'da öncelik bir veridir, bir politika değil.**
Politikaya dönüştüğü an M10'dur.

## Çıkış planı: nereye taşınacak

| Parça | M6'daki yeri | Nihai yeri |
|---|---|---|
| Bağlam değiştirme (register'lar, RSP) | kernel | **kernel** — mekanizma |
| "Şimdi bu thread'i çalıştır" | kernel | kernel — mekanizma |
| Hazır kuyruğu | kernel | user-space scheduler |
| Sıradakini seçme | kernel | user-space scheduler |
| Kuantum uzunluğu | kernel (sabit) | user-space scheduler |
| Öncelik yorumu | yok | user-space scheduler |

Taşıma M10'da ele alınacak ve M9'un IPC'sine bağlı. Taşındığında
kernel'de kalacak olan, bu ADR'nin ilk iki satırı.

Geçişi lokal tutmak için `sched` modülü bir sınırla ayrıldı: kernel'in
geri kalanı yalnızca `sched::yield_now()`, `sched::tick()` ve
`sched::current()` görüyor. Kuyruğun kendisi modülün içinde private.
Politika user-space'e taşındığında bu üç fonksiyonun gövdesi değişir,
çağıranları değişmez.

## Kuantum: 10 ms

LAPIC timer'ı 100 Hz'e kuruyoruz. Sayı keyfi ve öyle olduğunu yazmak
gerekiyor: ölçülmüş bir iş yükünden gelmiyor, "kabul testinin 100 tick'i
makul bir sürede toplaması" ve "preemption'ın gözle görülür olması"
arasında bir uzlaşma.

M6'nın bağlam değiştirme ölçümü (aşağıdaki `thread-test` senaryosu)
kuantumun maliyetini bağlama oturtuyor.

**Ölçülen:** 10 000 switch, 3 282 006 TSC çevrimi — **switch başına 328
çevrim**, 3 GHz'de ~109 ns. 10 ms'lik kuantum aynı hızda 30 milyon
çevrim, yani zamanlayıcının kendisi kuantumun **%0.001'i**. Vaat edilen
"onbinde bir" tavanının bir mertebe altında.

Rakamın ne olmadığı: emülasyon altında, sanal bir TSC ile, tek bir
çekirdekte, soğuk cache olmadan ölçüldü. QEMU'nun TSC'si gerçek
çevrimleri saymıyor; 328 bir büyüklük mertebesi, bir ölçüm değil.
Gerçek donanımda TLB ve cache etkileriyle daha yüksek çıkması beklenir.
Yine de iki şeyi kuruyor: kuantumun seçimi savunulabilir, ve M9'un IPC'si
karşılaştırılacak bir taban buluyor — IPC bir switch'ten pahalıysa,
mesaj geçirme yolunda switch'in kendisi dışında bir şey var demektir.

Bu oran bozulursa kuantum yeniden düşünülür.

## Yan etki: testler artık deterministik değil

Preemption'ın bedeli yalnızca çevrim değil. M5'e kadar her kabul testi
tam olarak aynı sayıları basıyordu, çünkü kernel'i hiçbir şey
bölemiyordu. M6 bunu bitirdi ve `run.py`'nin sabit sayı bekleyen
desenleri aralıklı olarak düşmeye başladı — yaklaşık 28 koşuda bir.

Somut örnek: ping-pong testi 200 turda duruyor, ama bazen 201 basıyor.
Her thread sayacı artırıp *sonra* bakıyor; 200'e çıkaran thread kendini
suspend ediyor, fakat partneri o an hâlâ hazır kuyruğunda olabiliyor ve
bir timer tick'i onu bir kez daha çalıştırıyor. İki thread'le aşım tam
olarak bir.

İki yanlış tepki vardı:

1. **Deseni `\d+` yapmak.** Sayıyı tamamen serbest bırakır, ve 202'yi de
   kabul ederdi — ki o, kendini suspend etmiş bir thread'in yeniden
   çalıştırıldığı anlamına gelir, yani `suspend`'in tek vaadinin
   çiğnenmesi.
2. **Testi tekrar koşup geçmesini beklemek.** Kararsız bir test,
   olmayan bir testten kötüdür: bir sonraki gerçek hatayı "yine o
   flaky test" diye eleyecek olan şey tam olarak budur.

**Yapılan:** belirsiz olan değeri değil, **sınırı** iddia etmek. Kernel
`rounds <= 201` assert ediyor, `run.py` deseni `20[01]`. Zamanlamadan
gelen belirsizlik kabul edildi; o belirsizliğin *sınırı* bir değişmez
olarak kaldı ve iki yerden birden kontrol ediliyor.

**Kural:** kesmeler açıldıktan sonra bir testin bastığı her sayı için
sorulacak soru, "bu kaç?" değil, "bu neyin arasında, ve neden?"

## Elenen alternatifler

**Yalnızca kooperatif yield, timer yok.** Daha az kod ve politika
sorusunu tamamen erteliyor. Ama preemption'ı test edemezdik, ve
preemption M6'nın asıl riski: IF'in ilk kez açılması. Timer'ı M10'a
ertelemek, kesmelerin ilk kez açılmasını da ertelerdi — ve o riski
scheduler'ın karmaşıklığıyla aynı anda almak daha kötü.

**Zamanlayıcıyı baştan bir capability'nin arkasına koymak.** "Scheduler
capability'si olan bileşen sırayı belirler." Cazip, ama M6'da o
bileşen yok; capability'nin arkasında yine kernel'in round-robin'i
olurdu. Soyutlamayı kullanıcısı olmadan eklemek, soyutlamayı yanlış
yerden kesme riski.

**Öncelik alanını M10'a ertelemek.** TCB'nin boyutu ADR 017'de
sabitlendi ve ölçülüyor; alanı sonradan eklemek tavanı değiştirmez ama
`SetPriority`'yi de ertelerdi, ve o invocation'ın var olması M10'un
işini kolaylaştırıyor.

## Sonuç

- Round-robin zamanlayıcı kernel'de, **çünkü user-space scheduler IPC
  gerektiriyor ve IPC M9'da.**
- Mekanizma (bağlam değiştirme) kernel'de **kalacak**; politika (sıra,
  kuantum, öncelik yorumu) M10'da taşınacak.
- `sched` modülünün dışarıya üç fonksiyonu var; geçiş o üçünün
  gövdesiyle sınırlı.
- Öncelik alanı var ama M6'da **kullanılmıyor** — veri, politika değil.
- Kuantum 10 ms, keyfi, ve ölçümle bağlama oturtuldu: switch 328 çevrim
  (emülasyonda), kuantumun %0.001'i.