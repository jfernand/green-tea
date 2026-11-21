// (c) Javier Fernandez @ JotaSoft 2026
// What the green-tea report needs beyond isss-template.typ. Ported from the JVM book's
// template and jotakey/docs/lib.typ.
#import "isss-template.typ": *

// Every listing is from this commit. check-snippets.py compares them with it.
#let pinned = "79b31d6"
#let repo-url = "https://github.com/jfernand/green-tea"
#let repo(path) = link(repo-url + "/blob/" + pinned + "/" + path, raw(path))

#let _badge(n) = box(baseline: 12%, circle(radius: 4.4pt, fill: brand-primary, stroke: none,
  align(center + horizon, text(font: font-mono, size: 5.8pt, weight: 700, fill: c-black, str(n)))))

// A listing from the repository: `lines` are the source lines it shows, as (from, to)
// ranges. In the listing, a line that is exactly `// …` marks a cut, and a line that starts
// with `»` continues the previous source line. `notes` are (line, [text]) pairs, shown as
// badges in the margin and explained under the listing.
#let srcfile(path, code, lines: (), notes: (), size: 7.2pt, breakable: false) = {
  let nums = ()
  for r in lines { for i in range(r.first(), r.last() + 1) { nums.push(i) } }
  let labels = ()
  let clean = ()
  let k = 0
  for l in code.text.split("\n") {
    if l.trim() == "// …" { labels.push(none); clean.push(l) }
    else if l.starts-with("»") { labels.push(none); clean.push(l.trim("»", at: start)) }
    else { labels.push(nums.at(k, default: none)); clean.push(l); k += 1 }
  }
  assert(k == nums.len(), message: path + ": listing has " + str(k) + " lines, ranges cover " + str(nums.len()))
  let marks = (:)
  for (i, nt) in notes.enumerate() {
    assert(nt.first() in nums, message: path + ": note on line " + str(nt.first()) + " outside the listing")
    marks.insert(str(nt.first()), i + 1)
  }
  let (first, last) = (lines.first().first(), lines.last().last())
  let url = repo-url + "/blob/" + pinned + "/" + path + "#L" + str(first) + "-L" + str(last)
  block(breakable: breakable, above: 12pt, below: if notes.len() > 0 { 6pt } else { 12pt }, width: 100%)[
    #block(width: 100%, below: 0pt, fill: c-black, stroke: (left: 3pt + brand-primary),
      inset: (left: 11pt, right: 9pt, y: 5pt),
      grid(columns: (1fr, auto), align: (left, right),
        link(url, text(font: font-mono, size: 6.6pt, fill: c-amber-300, path)),
        text(font: font-mono, size: 6.6pt, fill: c-steel-400, pinned + " · L" + str(first) + "–" + str(last))))
    #{
      show raw.where(block: true): it => block(width: 100%, above: 0pt, breakable: breakable,
        fill: c-ink, stroke: (left: 3pt + brand-primary), inset: (left: 11pt, right: 9pt, top: 7pt, bottom: 9pt))[
        #set par(justify: false, leading: 0.55em)
        #set text(font: font-mono, size: size, fill: c-paper)
        #it.lines.join(linebreak())
      ]
      show raw.line: it => {
        let lab = labels.at(it.number - 1, default: none)
        box(width: 2.6em, align(right, text(fill: c-steel-500, if lab != none { str(lab) })))
        h(1em)
        it.body
        if lab != none and str(lab) in marks { h(1fr); _badge(marks.at(str(lab))) }
      }
      raw(clean.join("\n"), block: true, lang: code.lang)
    }
  ]
  if notes.len() > 0 {
    set text(size: 8.4pt)
    set par(justify: false, leading: 0.6em)
    block(above: 6pt, below: 12pt, grid(columns: (auto, 1fr), column-gutter: 7pt, row-gutter: 0.75em,
      ..notes.enumerate().map(((i, nt)) => (_badge(i + 1), nt.last())).flatten()))
  }
}

// A recorded run. The body is a raw block; check-quotes.py finds each line in docs/output/.
#let output(title: "output", size: 7pt, body) = block(width: 100%, above: 11pt, below: 11pt, breakable: false,
  stroke: 0.6pt + border-strong, fill: c-white)[
  #block(width: 100%, fill: c-steel-100, inset: (x: 8pt, y: 3.5pt), below: 0pt, label(fill: c-steel-600, size: 6pt, title))
  #show raw.where(block: true): r => text(font: font-mono, size: size, fill: c-steel-800, r.lines.map(l => l.text).join("\n"))
  #set par(justify: false, leading: 0.5em)
  #block(inset: (x: 8pt, y: 7pt), above: 0pt, width: 100%, body)
]

#let takeaways(..items) = block(width: 100%, above: 15pt, below: 12pt, breakable: false, fill: c-ink, inset: 13pt)[
  #set text(fill: c-paper, size: 8.8pt)
  #show raw.where(block: false): it => box(fill: c-steel-800, inset: (x: 2.5pt), outset: (y: 2.5pt),
    text(font: font-mono, size: 7.8pt, fill: c-amber-200, it.text))
  #label(fill: brand-primary, size: 6.6pt)[Key takeaways]
  #v(3pt)
  #set list(marker: text(fill: brand-primary)[■], body-indent: 7pt)
  #for i in items.pos() [ - #i ]
]

// A byte-layout strip: fields = (name, printed size, relative width).
#let bytefield(caption: none, ..fields) = {
  let fs = fields.pos()
  block(width: 100%, above: 12pt, below: 12pt, breakable: false)[
    #grid(columns: fs.map(f => f.at(2) * 1fr), gutter: 0pt,
      ..fs.enumerate().map(((i, f)) => block(width: 100%, height: 30pt, inset: 4pt,
        fill: if calc.rem(i, 2) == 0 { c-ink } else { c-steel-800 }, stroke: (right: 0.6pt + c-steel-600),
        align(center + horizon)[
          #text(font: font-mono, size: 6.8pt, fill: c-paper, weight: 600)[#f.at(0)] \
          #text(font: font-mono, size: 6pt, fill: brand-primary)[#f.at(1)]
        ])))
    #if caption != none { v(3pt); label-sm[#caption] }
  ]
}

// A stack of labelled layers, top first: (title, detail, tone), tone "dark", "amber" or "light".
#let layers(caption: none, ..ls) = block(width: 100%, above: 12pt, below: 12pt, breakable: false)[
  #for l in ls.pos() {
    let (title, detail, tone) = l
    let (bg, fg) = if tone == "dark" { (c-ink, c-paper) } else if tone == "amber" { (brand-primary, c-black) } else { (c-steel-100, c-black) }
    block(width: 100%, fill: bg, inset: (x: 9pt, y: 6pt), below: 2pt,
      grid(columns: (34%, 1fr), column-gutter: 10pt,
        text(font: font-display, stretch: 75%, weight: 700, size: 11pt, fill: fg)[#upper(title)],
        text(size: 7.6pt, fill: fg)[#detail]))
  }
  #if caption != none { v(3pt); label-sm[#caption] }
]

// A part of the report: a band across the page, and an entry in the contents.
#let part(title, blurb) = {
  pagebreak(weak: true)
  place(hide(heading(level: 1, numbering: none, outlined: true, title)))
  v(50mm)
  block(width: 100%, fill: c-black, inset: 18pt)[
    #label(fill: brand-primary, size: 7pt)[Part]
    #v(8pt)
    #display(size: 34pt, fill: c-paper)[#title]
    #v(6pt)
    #line(length: 40%, stroke: 2.5pt + brand-primary)
    #v(6pt)
    #text(size: 9.4pt, fill: c-steel-300, blurb)
  ]
  pagebreak()
}

// `#show: appendices`: lettered headings, in the template's style.
#let appendices(body) = {
  set heading(numbering: "A.1", supplement: [Appendix])
  counter(heading).update(0)
  show heading.where(level: 1): it => block(width: 100%, above: 26pt, below: 12pt, breakable: false)[
    #line(length: 100%, stroke: 2.4pt + fg-primary)
    #v(6pt, weak: true)
    #grid(columns: (auto, 1fr), column-gutter: 12pt,
      block(fill: brand-primary, inset: (x: 7pt, y: 4pt),
        text(font: font-mono, weight: 600, size: 9pt, fill: c-black, context counter(heading).display("A"))),
      display(size: 25pt)[#it.body])
  ]
  show heading.where(level: 2): it => block(above: 17pt, below: 8pt, breakable: false)[
    #grid(columns: (auto, 1fr), column-gutter: 8pt, align: (left + horizon, left + horizon),
      label(fill: brand-deep, size: 7.4pt, context counter(heading).display("A.1")),
      text(font: font-display, stretch: 75%, weight: 700, size: 14.5pt, upper(it.body)))
    #v(3pt, weak: true)
    #line(length: 100%, stroke: 0.8pt + border-strong)
  ]
  body
}
