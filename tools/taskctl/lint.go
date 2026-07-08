package main

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

var linkRe = regexp.MustCompile(`\]\(([^)]+)\)`)

// cmdLint проверяет инварианты доски и архива. Жёсткие ошибки формата ловит
// уже разбор, здесь семантика: бакеты, сортировка, дубли, живость ссылок.
func cmdLint(root string) ([]string, error) {
	var finds []string
	bp, ap := boardPath(root), archivePath(root)
	b, err := LoadBoard(bp)
	if err != nil {
		return nil, err
	}
	arch, err := LoadArchive(ap)
	if err != nil {
		return nil, err
	}

	seen := map[string]string{}
	note := func(id, where string) {
		if prev, ok := seen[id]; ok {
			finds = append(finds, fmt.Sprintf("%s: дубль ID %s (уже есть: %s)", where, id, prev))
			return
		}
		seen[id] = where
	}
	for _, r := range b.Rows {
		note(r.ID, fmt.Sprintf("%s:%d", bp, r.LineIdx+1))
	}
	for _, r := range arch.Rows {
		note(r.ID, fmt.Sprintf("%s:%d", ap, r.LineIdx+1))
	}

	for _, r := range b.Rows {
		where := fmt.Sprintf("%s:%d: %s", bp, r.LineIdx+1, r.ID)
		if want := bucket(r.RTotal); r.P != want {
			finds = append(finds, fmt.Sprintf("%s: P=%s, а по R=%d должно быть %s", where, r.P, r.RTotal, want))
		}
		rank := fmt.Sprintf("%d+%d+%d+%d+%d", r.RParts[0], r.RParts[1], r.RParts[2], r.RParts[3], r.RParts[4])
		if _, _, err := parseRank(rank); err != nil {
			finds = append(finds, fmt.Sprintf("%s: разбивка ранга вне шкалы: %v", where, err))
		}
		finds = append(finds, checkLinks(root, bp, r.LineIdx, b.Lines[r.LineIdx])...)
	}

	rows := b.Sects[SectBacklog].Rows
	for i := 1; i < len(rows); i++ {
		prev, cur := rows[i-1], rows[i]
		if prev.RTotal < cur.RTotal || (prev.RTotal == cur.RTotal && prev.Num > cur.Num) {
			finds = append(finds, fmt.Sprintf("%s:%d: Backlog не отсортирован: %s (R=%d) стоит ниже положенного",
				bp, cur.LineIdx+1, cur.ID, cur.RTotal))
		}
	}

	for _, r := range arch.Rows {
		where := fmt.Sprintf("%s:%d: %s", ap, r.LineIdx+1, r.ID)
		if !dateRe.MatchString(r.Cells[4]) {
			finds = append(finds, fmt.Sprintf("%s: дата закрытия %q не вида ГГГГ-ММ-ДД", where, r.Cells[4]))
		}
		finds = append(finds, checkLinks(root, ap, r.LineIdx, arch.Lines[r.LineIdx])...)
	}
	return finds, nil
}

// checkLinks проверяет, что локальные markdown-ссылки строки ведут на
// существующие файлы. Пути в доске и архиве относительны docs/.
func checkLinks(root, file string, lineIdx int, line string) []string {
	var finds []string
	for _, m := range linkRe.FindAllStringSubmatch(line, -1) {
		target := m[1]
		if strings.HasPrefix(target, "http://") || strings.HasPrefix(target, "https://") ||
			strings.HasPrefix(target, "mailto:") || strings.HasPrefix(target, "#") {
			continue
		}
		if i := strings.IndexByte(target, '#'); i >= 0 {
			target = target[:i]
		}
		if target == "" {
			continue
		}
		if _, err := os.Stat(filepath.Join(root, "docs", target)); err != nil {
			finds = append(finds, fmt.Sprintf("%s:%d: битая ссылка %s (нет файла docs/%s)",
				file, lineIdx+1, m[1], target))
		}
	}
	return finds
}
