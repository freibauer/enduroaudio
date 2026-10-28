#set document(title: "Enduro Audio – leise Stellen anheben")
#set page(paper: "a4", margin: (x: 2.2cm, y: 2.2cm), numbering: "1")
#set text(lang: "de", size: 10.5pt)
#set heading(numbering: "1.")
#set par(justify: true)
#show raw.where(block: false): box.with(fill: luma(240), inset: (x: 3pt), outset: (y: 3pt), radius: 2pt)

#align(center)[
  #text(size: 22pt, weight: "bold")[Enduro Audio]
  #v(-4pt)
  #text(size: 13pt)[Leise Stellen in Livestream-Aufnahmen automatisch anheben]
]

#v(8pt)

Enduro Audio ist ein Windows-Programm (eine einzelne `.exe`, ohne Installation). Es gleicht
in Livestream-Aufzeichnungen (MP4) leise Abschnitte an, zum Beispiel nach Pausen. Das Video
wird unverändert übernommen. Nur die Tonspur wird bearbeitet und neu kodiert.

#figure(
  image("gui.png", width: 100%),
  caption: [Die Oberfläche nach der Analyse: Sprachpegel vorher (rot) und nachher (blau) sowie die angewendete Verstärkung.],
)

= Bedienung

+ `enduroaudio.exe` per Doppelklick starten. Eine Installation ist nicht nötig.
+ Mit *Öffnen …* eine MP4-Datei wählen oder die Datei ins Fenster ziehen.
  Die Ausgabedatei wird automatisch vorgeschlagen (Name mit der Endung `_laut.mp4`).
+ *1. Analysieren* anklicken. Das Programm misst den Pegel der ganzen Datei. Danach erscheinen
  der Pegelverlauf und eine Liste der erkannten leisen Stellen.
+ Bei Bedarf die Regler anpassen. Diagramm und Liste aktualisieren sich sofort, ohne dass
  neu analysiert werden muss.
+ *2. Verarbeiten & speichern* anklicken. Das Ergebnis wird als neue MP4-Datei
  gespeichert. Die Originaldatei bleibt unverändert.

Beide Schritte lassen sich jederzeit mit *Abbrechen* stoppen.

= Einstellungen

#table(
  columns: (auto, auto, 1fr),
  inset: 6pt,
  stroke: 0.5pt + luma(180),
  fill: (_, y) => if y == 0 { luma(230) },
  [*Regler*], [*Standard*], [*Bedeutung*],
  [Ziel-Sprachpegel], [−21 dBFS], [Auf diesen Pegel wird die Sprache geregelt (RMS ohne Pausen).],
  [Regelfenster], [10 s], [Zeitraum, über den der Pegel gemittelt wird. Kürzer gleicht leise Stellen schneller aus, ebnet aber auch die Betonung innerhalb von Sätzen stärker ein.],
  [Max. Anhebung], [20 dB], [Stärker werden leise Stellen nie angehoben.],
  [Max. Absenkung], [8 dB], [Stärker werden zu laute Stellen nie abgesenkt.],
  [Stille-Schwelle], [−52 dBFS], [Was leiser ist, gilt als Pause. Pausen fließen nicht in die Messung ein und werden nicht hochgezogen.],
  [Limiter-Grenze], [−1 dBFS], [Spitzen werden auf diesen Wert begrenzt, damit durch die Anhebung nichts übersteuert.],
  [AAC-Bitrate], [192 kbit/s], [Qualität der neu kodierten Tonspur (Stereo; Mono erhält die Hälfte).],
)

= Wie der Filter funktioniert

Das Verfahren wurde aus einer Analyse einer echten Aufnahme abgeleitet (Enduro-Rene-Livestream,
3 h 06 min, AAC 44,1 kHz). Dabei fielen zwei Probleme auf:

- *Kurze, sehr leise Passagen* von 10 bis 80 Sekunden, die bis zu 20 dB leiser sind
  als der Rest, typischerweise direkt nach einer Unterbrechung.
- *Langsame Pegeldrifts* über viele Minuten von etwa ±4 dB. Der Sprachpegel schwankte in
  30-Sekunden-Blöcken insgesamt um ±3 dB.

Ein normaler Kompressor oder eine automatische Lautstärkeregelung reagiert immer verzögert.
Gerade nach einer Pause ist der Ton dann erst einige Sekunden zu leise. Enduro Audio
analysiert deshalb zuerst die ganze Datei und regelt danach vorausschauend:

+ *Messung:* Das Signal wird in 50-ms-Abschnitte zerlegt. Für jeden Abschnitt wird die Energie
  gemessen, nach einem 80-Hz-Hochpass, damit Rumpeln nicht mitgezählt wird.
+ *Sprachpegel:* Für jeden Zeitpunkt wird der mittlere Pegel aller aktiven Abschnitte in einem
  gaußförmigen Fenster *um* diesen Zeitpunkt berechnet. Stille (Noise-Gate, Pausen) sowie
  Geräusche mehr als 20 dB unter dem lokalen Pegel werden ignoriert.
+ *Verstärkung:* Die Verstärkung ist Zielpegel minus Sprachpegel, begrenzt durch die
  maximale Anhebung und Absenkung, und wird zusätzlich über 1 s geglättet. Weil das Fenster
  auch in die Zukunft schaut, stimmt die Verstärkung nach einer Pause sofort.
+ *Limiter:* Ein Lookahead-Limiter (5 ms Vorlauf, 150 ms Release) fängt Spitzen ab.
+ *Ausgabe:* Die Tonspur wird mit dem Fraunhofer-AAC-Encoder (FDK AAC) neu kodiert. Die
  Videospur wird Sample für Sample und bitgenau in die neue MP4-Datei kopiert.

In der Beispielaufnahme sinkt die Schwankung des Sprachpegels dadurch von etwa ±3 dB auf
etwa ±0,4 dB. Die Verarbeitung der drei Stunden dauerte auf dem Testrechner gut zwei Minuten.

= Einschränkungen

- Eingabe: MP4, M4A, M4V und MOV mit AAC-Tonspur. Fragmentierte MP4-Dateien werden nicht
  unterstützt.
- Es wird die erste Tonspur bearbeitet. Weitere Tonspuren werden nicht übernommen.
- Hintergrundgeräusche in leisen Passagen werden zusammen mit der Sprache angehoben.
  Bei Bedarf hilft eine geringere maximale Anhebung.

= Selbst bauen

Gebaut wird unter Linux oder WSL mit Rust und MinGW:

```sh
rustup target add x86_64-pc-windows-gnu
sudo apt install gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64
cargo build --release --target x86_64-pc-windows-gnu
```

Das Ergebnis liegt in `target/x86_64-pc-windows-gnu/release/enduroaudio.exe`. Es ist statisch
gelinkt und benötigt nur Windows-Systembibliotheken.

Eine Kommandozeilenversion ohne Oberfläche, etwa für Linux, entsteht mit
`cargo build --release --no-default-features`. Aufruf:
`enduroaudio --cli eingabe.mp4 [ausgabe.mp4]`.
