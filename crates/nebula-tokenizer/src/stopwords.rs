//! 中英文停用词表。命中停用词的表项不进入关键词与关键点。

use std::collections::HashSet;
use std::sync::OnceLock;

fn set(words: &[&'static str]) -> HashSet<&'static str> {
    words.iter().copied().collect()
}

fn zh() -> HashSet<&'static str> {
    set(&[
        "的", "了", "和", "是", "就", "都", "而", "及", "与", "着", "或", "一个", "没有", "我们",
        "你们", "他们", "她们", "它们", "自己", "这", "那", "这个", "那个", "这些", "那些", "什么",
        "怎么", "怎样", "如何", "因为", "所以", "但是", "不过", "虽然", "如果", "就是", "不是",
        "不会", "不能", "可以", "应该", "需要", "已经", "正在", "将要", "还有", "以及", "并且",
        "或者", "因为", "因此", "然后", "接着", "首先", "其次", "最后", "例如", "比如", "也就是",
        "来说", "对于", "关于", "由于", "除了", "之一", "之后", "之前", "时候", "这样", "那样",
        "一样", "不同", "一直", "一定", "一般", "一起", "一些", "一下", "一切", "其他", "另外",
        "其实", "可能", "也许", "当然", "确实", "真的", "觉得", "认为", "知道", "发现", "看到",
        "听说", "出现", "发生", "成为", "变成", "做出", "进行", "通过", "根据", "按照", "随着",
        "同时", "此外", "总的来说", "总的来看", "一般来", "讲", "说", "做", "弄", "搞", "打",
        "问题", "东西", "事情", "样子", "方式", "方法", "方面", "情况", "地方", "时间", "今天",
        "昨天", "明天", "现在", "以前", "以后", "起来", "出来", "下去", "下来", "过来", "过去",
        "非常", "特别", "比较", "更加", "最好", "较大", "最大", "最小", "很多", "许多", "不少",
        "有些", "有的", "每个", "各种", "各位", "大家", "别人", "人家", "多少", "几个", "第一",
        "第二", "第三", "一是", "二是", "三是", "您好", "你好", "谢谢", "感谢", "请问", "是否",
    ])
}

fn en() -> HashSet<&'static str> {
    set(&[
        "the", "a", "an", "and", "or", "but", "if", "then", "else", "when", "while", "of", "at",
        "by", "for", "with", "about", "against", "between", "into", "through", "during", "before",
        "after", "above", "below", "to", "from", "up", "down", "in", "out", "on", "off", "over",
        "under", "again", "further", "once", "here", "there", "all", "any", "both", "each", "few",
        "more", "most", "other", "some", "such", "no", "nor", "not", "only", "own", "same", "so",
        "than", "too", "very", "can", "will", "just", "should", "now", "is", "am", "are", "was",
        "were", "be", "been", "being", "have", "has", "had", "having", "do", "does", "did", "doing",
        "would", "could", "ought", "i", "you", "he", "she", "it", "we", "they", "them", "his",
        "her", "its", "our", "their", "this", "that", "these", "those", "as", "because", "until",
        "how", "what", "which", "who", "whom", "why", "where", "also", "however", "therefore",
        "thus", "hence", "moreover", "furthermore", "namely", "e.g", "i.e", "etc", "via", "per",
        "vs", "using", "used", "use", "uses", "one", "two", "three", "get", "got", "make", "made",
        "way", "thing", "things", "something", "anything", "everything", "nothing", "really",
        "quite", "much", "many", "lot", "lots", "kind", "sort", "part", "parts", "well", "like",
    ])
}

pub fn is_stopword(term: &str) -> bool {
    static STOPWORDS: OnceLock<(HashSet<&'static str>, HashSet<&'static str>)> = OnceLock::new();
    let (zh, en) = STOPWORDS.get_or_init(|| (zh(), en()));
    if let Some(first) = term.chars().next() {
        if is_cjk(first) {
            zh.contains(term)
        } else {
            en.contains(term)
        }
    } else {
        false
    }
}

/// 是否 CJK 统一表意文字(含扩展)。
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF      // CJK Unified Ideographs
        | 0x3400..=0x4DBF    // Ext A
        | 0x20000..=0x2A6DF  // Ext B
        | 0xF900..=0xFAFF    // Compatibility Ideographs
    )
}

/// 以 Unicode 标点/空白为主的词视为噪声(如 jieba 切出的 "。"、" ")。
pub fn is_noise(term: &str) -> bool {
    term.chars().all(|c| c.is_whitespace() || c.is_ascii_punctuation()
        || matches!(c, '，'|'。'|'！'|'？'|'；'|'：'|'、'|'“'|'”'|'‘'|'’'|'（'|'）'|'《'|'》'|'—'|'…'|'·'|'【'|'】'|'「'|'」'))
}
